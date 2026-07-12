/*
 * Flat C bridge over the Sony Camera Remote SDK (CrSDK). See bridge.h.
 *
 * One SonyCamera wraps a single session and implements IDeviceCallback so it can
 * block sn_connect() until OnConnected and sn_capture() until OnCompleteDownload.
 */
#include "bridge.h"

#include "CRSDK/CameraRemote_SDK.h"
#include "CRSDK/ICrCameraObjectInfo.h"
#include "CRSDK/IDeviceCallback.h"
#include "CRSDK/CrDeviceProperty.h"
#include "CRSDK/CrCommandData.h"
#include "CRSDK/CrImageDataBlock.h"

#include <atomic>
#include <chrono>
#include <condition_variable>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <mutex>
#include <string>
#include <thread>
#include <vector>

#ifdef _WIN32
#include <windows.h>
#endif

namespace SDK = SCRSDK;

// The Cr integer/char typedefs (CrChar, CrInt8u, CrInt32, …) live at GLOBAL
// scope (CrTypes.h has no namespace); only CrDeviceHandle, CrError, the enums and
// the classes are in SCRSDK. CrChar is wchar_t on Windows (the DLL is built
// UNICODE) and char elsewhere.
using tchar   = CrChar;
using tstring = std::basic_string<CrChar>;

// ---------------------------------------------------------------------------
// String / path helpers
// ---------------------------------------------------------------------------

static std::string to_utf8(const tchar* s) {
    if (!s) return std::string();
#ifdef _WIN32
    int len = WideCharToMultiByte(CP_UTF8, 0, s, -1, nullptr, 0, nullptr, nullptr);
    if (len <= 0) return std::string();
    std::string out(static_cast<size_t>(len - 1), '\0');
    WideCharToMultiByte(CP_UTF8, 0, s, -1, out.data(), len, nullptr, nullptr);
    return out;
#else
    return std::string(s);
#endif
}

static tstring from_utf8(const std::string& s) {
#ifdef _WIN32
    if (s.empty()) return tstring();
    int len = MultiByteToWideChar(CP_UTF8, 0, s.c_str(), -1, nullptr, 0);
    if (len <= 0) return tstring();
    tstring out(static_cast<size_t>(len - 1), L'\0');
    MultiByteToWideChar(CP_UTF8, 0, s.c_str(), -1, out.data(), len);
    return out;
#else
    return s;
#endif
}

static void copy_cstr(char* dst, size_t cap, const std::string& src) {
    if (cap == 0) return;
    size_t n = src.size() < cap - 1 ? src.size() : cap - 1;
    std::memcpy(dst, src.data(), n);
    dst[n] = '\0';
}

static std::string hex_encode(const uint8_t* data, size_t len) {
    static const char* H = "0123456789abcdef";
    std::string out;
    out.reserve(len * 2);
    for (size_t i = 0; i < len; ++i) {
        out.push_back(H[data[i] >> 4]);
        out.push_back(H[data[i] & 0x0F]);
    }
    return out;
}

// Directory where the SDK drops captured stills; read back then deleted.
static tstring g_tempdir;

static void ensure_tempdir() {
    if (!g_tempdir.empty()) return;
#ifdef _WIN32
    wchar_t buf[MAX_PATH];
    DWORD n = GetTempPathW(MAX_PATH, buf);
    tstring dir(buf, n);
    dir += L"toucan_sony\\";
    CreateDirectoryW(dir.c_str(), nullptr);
    g_tempdir = dir;
#else
    const char* base = std::getenv("TMPDIR");
    std::string dir = base && *base ? std::string(base) : std::string("/tmp");
    if (dir.back() != '/') dir += '/';
    dir += "toucan_sony/";
    ::system((std::string("mkdir -p '") + dir + "'").c_str());
    g_tempdir = dir;
#endif
}

// Reads a whole file into a malloc'd buffer. Returns true on success.
static bool read_whole_file(const tstring& path, uint8_t** out, uint32_t* size) {
#ifdef _WIN32
    FILE* f = _wfopen(path.c_str(), L"rb");
#else
    FILE* f = std::fopen(path.c_str(), "rb");
#endif
    if (!f) return false;
    std::fseek(f, 0, SEEK_END);
    long len = std::ftell(f);
    std::fseek(f, 0, SEEK_SET);
    if (len <= 0) { std::fclose(f); return false; }
    uint8_t* buf = static_cast<uint8_t*>(std::malloc(static_cast<size_t>(len)));
    if (!buf) { std::fclose(f); return false; }
    size_t rd = std::fread(buf, 1, static_cast<size_t>(len), f);
    std::fclose(f);
    if (rd != static_cast<size_t>(len)) { std::free(buf); return false; }
    *out = buf;
    *size = static_cast<uint32_t>(len);
    return true;
}

static void delete_file(const tstring& path) {
#ifdef _WIN32
    _wremove(path.c_str());
#else
    std::remove(path.c_str());
#endif
}

// Byte width of one element for a CrDataType (0 for string / unsupported).
static uint32_t element_width(uint32_t value_type) {
    switch (value_type & 0x000F) {
        case 0x0001: return 1; // UInt8 / Int8
        case 0x0002: return 2; // UInt16 / Int16
        case 0x0003: return 4; // UInt32 / Int32
        case 0x0004: return 8; // UInt64 / Int64
        default:     return 0;
    }
}

// ---------------------------------------------------------------------------
// SonyCamera: one session + its IDeviceCallback
// ---------------------------------------------------------------------------

class SonyCamera : public SDK::IDeviceCallback {
public:
    SDK::CrDeviceHandle handle = 0;
    SDK::ICrEnumCameraObjectInfo* enum_list = nullptr; // kept alive so `info` stays valid

    std::mutex mtx;
    std::condition_variable cv;
    bool connected     = false;
    bool conn_failed   = false;
    bool download_done = false;
    tstring download_path;

    // --- IDeviceCallback ---
    void OnConnected(SDK::DeviceConnectionVersioin) override {
        std::lock_guard<std::mutex> lk(mtx);
        connected = true;
        cv.notify_all();
    }
    void OnDisconnected(CrInt32u) override {
        std::lock_guard<std::mutex> lk(mtx);
        connected = false;
        cv.notify_all();
    }
    void OnError(CrInt32u) override {
        std::lock_guard<std::mutex> lk(mtx);
        if (!connected) conn_failed = true; // surface connect-time failures
        cv.notify_all();
    }
    void OnCompleteDownload(tchar* filename, CrInt32u) override {
        std::lock_guard<std::mutex> lk(mtx);
        download_path = filename ? tstring(filename) : tstring();
        download_done = true;
        cv.notify_all();
    }
    void OnNotifyPostViewImage(tchar*, CrInt32u) override {}
    void OnWarning(CrInt32u) override {}
    void OnPropertyChanged() override {}
    void OnLvPropertyChanged() override {}
};

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

static std::atomic<bool> g_inited{false};

// Reads one property's current raw value; returns false if unavailable.
static bool get_current_value(SDK::CrDeviceHandle handle, uint32_t code, uint64_t* out) {
    CrInt32u codes[1] = { code };
    SDK::CrDeviceProperty* props = nullptr;
    CrInt32 num = 0;
    if (SDK::GetSelectDeviceProperties(handle, 1, codes, &props, &num) != SDK::CrError_None
        || num < 1 || !props) {
        return false;
    }
    *out = props[0].GetCurrentValue();
    SDK::ReleaseDeviceProperties(handle, props);
    return true;
}

// Sets one device property, resolving its value type from the camera so callers
// only need the property code + raw value.
//
// SetDeviceProperty is asynchronous: the body applies the change a moment later
// and signals it via OnPropertyChanged. We block until the value actually takes
// (or the body snaps it to a nearby valid value) so the caller's follow-up read
// reflects the new state instead of the stale one — otherwise the UI glitches.
// Bounded, so a value the body silently rejects can't hang the request.
static SDK::CrError set_property(SDK::CrDeviceHandle handle, uint32_t code, uint64_t value) {
    CrInt32u codes[1] = { code };
    SDK::CrDeviceProperty* props = nullptr;
    CrInt32 num = 0;
    if (SDK::GetSelectDeviceProperties(handle, 1, codes, &props, &num) != SDK::CrError_None
        || num < 1 || !props) {
        return SDK::CrError_Generic;
    }
    SDK::CrDataType value_type = props[0].GetValueType();
    uint64_t old_value = props[0].GetCurrentValue();
    SDK::ReleaseDeviceProperties(handle, props);

    SDK::CrDeviceProperty prop;
    prop.SetCode(code);
    prop.SetValueType(value_type);
    prop.SetCurrentValue(value);
    SDK::CrError err = SDK::SetDeviceProperty(handle, &prop);
    if (err != SDK::CrError_None) {
        return err;
    }

    for (int i = 0; i < 40; ++i) { // up to ~2 s
        std::this_thread::sleep_for(std::chrono::milliseconds(50));
        uint64_t now = old_value;
        if (get_current_value(handle, code, &now) && (now == value || now != old_value)) {
            break;
        }
    }
    return SDK::CrError_None;
}

// ---------------------------------------------------------------------------
// C API
// ---------------------------------------------------------------------------

extern "C" {

int sn_init(void) {
    if (g_inited.load()) return SN_OK;
    bool ok = SDK::Init(0);
    std::fprintf(stderr, "[sony-bridge] SDK::Init -> %s (SDK version 0x%08X)\n",
                 ok ? "OK" : "FAIL", SDK::GetSDKVersion());
    if (!ok) return SN_ERR;
    ensure_tempdir();
    g_inited.store(true);
    return SN_OK;
}

void sn_release(void) {
    if (!g_inited.load()) return;
    SDK::Release();
    g_inited.store(false);
}

int sn_list_devices(SnDeviceInfo* out, int capacity) {
    if (sn_init() != SN_OK) return SN_ERR;

    // Full 3 s scan: the α7 IV (and others) need it to be discovered over USB.
    // The Rust side never calls this inline for /cameras — it serves a cached list
    // refreshed on the idle SDK thread — so the scan time doesn't stall the route.
    SDK::ICrEnumCameraObjectInfo* list = nullptr;
    if (SDK::EnumCameraObjects(&list, 3) != SDK::CrError_None || !list) {
        return 0; // no cameras (or enumeration failed) — treat as empty
    }

    int total = static_cast<int>(list->GetCount());
    int n = total < capacity ? total : capacity;
    for (int i = 0; i < n; ++i) {
        const SDK::ICrCameraObjectInfo* ci = list->GetCameraObjectInfo(i);
        if (!ci) { n = i; break; }
        copy_cstr(out[i].model, SN_MAX_MODEL, to_utf8(ci->GetModel()));
        copy_cstr(out[i].id, SN_MAX_ID, hex_encode(ci->GetId(), ci->GetIdSize()));
        copy_cstr(out[i].conn_type, SN_MAX_CONN, to_utf8(ci->GetConnectionTypeName()));
    }
    list->Release();
    return n;
}

void* sn_connect(const char* native_id) {
    if (sn_init() != SN_OK) return nullptr;

    SDK::ICrEnumCameraObjectInfo* list = nullptr;
    if (SDK::EnumCameraObjects(&list) != SDK::CrError_None || !list) {
        return nullptr;
    }

    const SDK::ICrCameraObjectInfo* match = nullptr;
    int total = static_cast<int>(list->GetCount());
    for (int i = 0; i < total; ++i) {
        const SDK::ICrCameraObjectInfo* ci = list->GetCameraObjectInfo(i);
        if (!ci) continue;
        if (hex_encode(ci->GetId(), ci->GetIdSize()) == native_id) {
            match = ci;
            break;
        }
    }
    if (!match) { list->Release(); return nullptr; }

    SonyCamera* cam = new SonyCamera();
    cam->enum_list = list; // keep the list (and `match`) alive for the session

    // Connect defaults to CrSdkControlMode_Remote + reconnect ON. The call returns
    // immediately; the session is ready only once OnConnected fires.
    SDK::CrError err = SDK::Connect(
        const_cast<SDK::ICrCameraObjectInfo*>(match), cam, &cam->handle);
    if (err != SDK::CrError_None) {
        list->Release();
        delete cam;
        return nullptr;
    }

    {
        std::unique_lock<std::mutex> lk(cam->mtx);
        cam->cv.wait_for(lk, std::chrono::seconds(15),
                         [&] { return cam->connected || cam->conn_failed; });
        if (!cam->connected) {
            lk.unlock();
            SDK::ReleaseDevice(cam->handle);
            list->Release();
            delete cam;
            return nullptr;
        }
    }

    // Route stills to the PC (and card) so capture downloads reliably, point the
    // SDK at our temp dir, and enable the live-view stream.
    set_property(cam->handle, SDK::CrDeviceProperty_StillImageStoreDestination,
                 SDK::CrStillImageStoreDestination_HostPCAndMemoryCard);
    tstring prefix; // empty → SDK default
    SDK::SetSaveInfo(cam->handle, const_cast<tchar*>(g_tempdir.c_str()),
                     const_cast<tchar*>(prefix.c_str()), SDK::CrSETSAVEINFO_AUTO_NUMBER);
    SDK::SetDeviceSetting(cam->handle, SDK::Setting_Key_EnableLiveView,
                          SDK::CrDeviceSetting_Enable);

    return cam;
}

void sn_disconnect(void* handle) {
    SonyCamera* cam = static_cast<SonyCamera*>(handle);
    if (!cam) return;
    SDK::Disconnect(cam->handle);
    SDK::ReleaseDevice(cam->handle);
    if (cam->enum_list) cam->enum_list->Release();
    delete cam;
}

int sn_get_parameters(void* handle, SnParam* out, int capacity) {
    SonyCamera* cam = static_cast<SonyCamera*>(handle);
    if (!cam) return SN_ERR;

    SDK::CrDeviceProperty* props = nullptr;
    CrInt32 num = 0;
    if (SDK::GetDeviceProperties(cam->handle, &props, &num) != SDK::CrError_None || !props) {
        return SN_ERR;
    }

    int n = static_cast<int>(num) < capacity ? static_cast<int>(num) : capacity;
    for (int i = 0; i < n; ++i) {
        SDK::CrDeviceProperty& p = props[i];
        SnParam& o = out[i];
        o.code       = p.GetCode();
        o.current    = p.GetCurrentValue();
        o.writable   = p.IsSetEnableCurrentValue() ? 1 : 0;
        o.value_type = p.GetValueType();
        o.num_options = 0;

        uint32_t w = element_width(o.value_type);
        const uint8_t* vals = p.GetValues();
        uint32_t vsize = p.GetValueSize();
        if (w != 0 && vals && vsize >= w) {
            int count = static_cast<int>(vsize / w);
            if (count > SN_MAX_OPTIONS) count = SN_MAX_OPTIONS;
            for (int k = 0; k < count; ++k) {
                uint64_t v = 0;
                std::memcpy(&v, vals + static_cast<size_t>(k) * w, w);
                o.options[k] = static_cast<int64_t>(v);
            }
            o.num_options = count;
        }
    }
    SDK::ReleaseDeviceProperties(cam->handle, props);
    return n;
}

int sn_set_parameter(void* handle, uint32_t code, uint64_t value) {
    SonyCamera* cam = static_cast<SonyCamera*>(handle);
    if (!cam) return SN_ERR;
    return set_property(cam->handle, code, value);
}

int sn_set_iso_auto(void* handle, int enable) {
    SonyCamera* cam = static_cast<SonyCamera*>(handle);
    if (!cam) return SN_ERR;

    const uint32_t ISO_AUTO = 0x00FFFFFF; // bits 0-23 all set = AUTO
    if (enable) {
        return set_property(cam->handle, SDK::CrDeviceProperty_IsoSensitivity, ISO_AUTO);
    }

    // Leaving auto: the ISO property has no separate toggle, so pick the lowest
    // concrete value the body currently offers.
    CrInt32u code = SDK::CrDeviceProperty_IsoSensitivity;
    SDK::CrDeviceProperty* props = nullptr;
    CrInt32 num = 0;
    if (SDK::GetSelectDeviceProperties(cam->handle, 1, &code, &props, &num) != SDK::CrError_None
        || num < 1 || !props) {
        return SDK::CrError_Generic;
    }
    uint32_t w = element_width(props[0].GetValueType());
    const uint8_t* vals = props[0].GetValues();
    uint32_t vsize = props[0].GetValueSize();
    bool found = false;
    uint32_t lowest = 0;
    if (w != 0 && vals) {
        for (uint32_t off = 0; off + w <= vsize; off += w) {
            uint64_t v = 0;
            std::memcpy(&v, vals + off, w);
            uint32_t iso = static_cast<uint32_t>(v) & 0x00FFFFFF;
            if (iso == 0x00FFFFFF) continue; // skip AUTO
            if (!found || iso < (lowest & 0x00FFFFFF)) {
                lowest = static_cast<uint32_t>(v);
                found = true;
            }
        }
    }
    SDK::ReleaseDeviceProperties(cam->handle, props);
    if (!found) return SDK::CrError_Generic;
    return set_property(cam->handle, code, lowest);
}

int sn_get_live_view(void* handle, uint8_t** out, uint32_t* size) {
    SonyCamera* cam = static_cast<SonyCamera*>(handle);
    if (!cam) return SN_ERR;

    SDK::CrImageInfo info;
    if (SDK::GetLiveViewImageInfo(cam->handle, &info) != SDK::CrError_None) {
        return SN_NOT_READY;
    }
    uint32_t buf_size = info.GetBufferSize();
    if (buf_size < 1) return SN_NOT_READY;

    std::vector<CrInt8u> buffer(buf_size);
    SDK::CrImageDataBlock img;
    img.SetSize(buf_size);
    img.SetData(buffer.data());
    if (SDK::GetLiveViewImage(cam->handle, &img) != SDK::CrError_None) {
        return SN_NOT_READY;
    }
    uint32_t img_size = img.GetImageSize();
    if (img_size == 0) return SN_NOT_READY;

    uint8_t* result = static_cast<uint8_t*>(std::malloc(img_size));
    if (!result) return SN_ERR;
    std::memcpy(result, img.GetImageData(), img_size);
    *out = result;
    *size = img_size;
    return SN_OK;
}

int sn_capture(void* handle, uint8_t** out, uint32_t* size) {
    SonyCamera* cam = static_cast<SonyCamera*>(handle);
    if (!cam) return SN_ERR;

    {
        std::lock_guard<std::mutex> lk(cam->mtx);
        cam->download_done = false;
        cam->download_path.clear();
    }

    // Half-press + full-press shutter, then release.
    SDK::SendCommand(cam->handle, SDK::CrCommandId_Release, SDK::CrCommandParam_Down);
    std::this_thread::sleep_for(std::chrono::milliseconds(35));
    SDK::SendCommand(cam->handle, SDK::CrCommandId_Release, SDK::CrCommandParam_Up);

    tstring path;
    {
        std::unique_lock<std::mutex> lk(cam->mtx);
        if (!cam->cv.wait_for(lk, std::chrono::seconds(20),
                              [&] { return cam->download_done; })) {
            return SN_ERR; // timed out waiting for the download
        }
        path = cam->download_path;
    }
    if (path.empty()) return SN_ERR;

    bool ok = read_whole_file(path, out, size);
    delete_file(path);
    return ok ? SN_OK : SN_ERR;
}

void sn_free(uint8_t* p) {
    std::free(p);
}

} // extern "C"
