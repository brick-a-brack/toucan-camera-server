/*
 * Emscripten binding of libgphoto2 for the toucan-camera-server WebAssembly
 * build. libgphoto2 reaches the camera through libusb's WebUSB backend.
 *
 * Derived from web-gphoto2's src/api.cpp:
 *   Copyright 2021 Google LLC
 *   https://github.com/GoogleChromeLabs/web-gphoto2
 *   https://github.com/edward-io/web-gphoto2 (fork updated to newer libgphoto2)
 *
 * Changes from upstream: several cameras at once (autodetect + opening a given
 * port, instead of "the first camera found"), raw bytes instead of Blob/File,
 * and errors returned as `{ error, code }` values instead of C++ exceptions, so
 * the module builds without -fexceptions and callers can tell libgphoto2's
 * result codes apart (the toucan backend needs GP_ERROR vs GP_ERROR_NOT_SUPPORTED
 * to detect a body that refuses to drive its focus motor).
 *
 * This library is free software; you can redistribute it and/or
 * modify it under the terms of the GNU Lesser General Public
 * License as published by the Free Software Foundation; either
 * version 2.1 of the License, or (at your option) any later version.
 *
 * This library is distributed in the hope that it will be useful,
 * but WITHOUT ANY WARRANTY; without even the implied warranty of
 * MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the GNU
 * Lesser General Public License for more details.
 */

#include <emscripten.h>
#include <emscripten/bind.h>
#include <emscripten/val.h>
#include <gphoto2/gphoto2.h>
#include <gphoto2/gphoto2-version.h>

#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <iostream>
#include <memory>
#include <string>

using emscripten::val;

namespace {

template <typename T, auto Deleter>
using gp_ptr =
    std::unique_ptr<T, std::integral_constant<decltype(Deleter), Deleter>>;

// A libgphoto2 failure, carried as a value rather than thrown.
val error_value(int code, const std::string &what) {
  val result = val::object();
  std::string message = what + ": " + gp_result_as_string(code);
  result.set("error", message);
  result.set("code", code);
  return result;
}

val error_message(const std::string &message) {
  val result = val::object();
  result.set("error", message);
  result.set("code", GP_ERROR);
  return result;
}

void log_error(GPContext *, const char *text, void *) {
  std::cerr << "[libgphoto2] " << text << std::endl;
}

GPContext *new_context() {
  GPContext *context = gp_context_new();
  gp_context_set_error_func(context, log_error, nullptr);
  return context;
}

// Copies `size` bytes out of the wasm heap into a standalone Uint8Array. A view
// on the heap would be invalidated by the next memory growth, and with pthreads
// the heap is a SharedArrayBuffer, which most web APIs refuse.
val copy_bytes(const char *data, unsigned long size) {
  val view = val(emscripten::typed_memory_view(size, data));
  return val::global("Uint8Array").new_(view);
}

val file_to_js(CameraFile *file) {
  const char *data = nullptr;
  unsigned long size = 0;
  int ret = gp_file_get_data_and_size(file, &data, &size);
  if (ret < GP_OK) return error_value(ret, "gp_file_get_data_and_size");

  const char *mime = nullptr;
  gp_file_get_mime_type(file, &mime);

  val result = val::object();
  result.set("mime", std::string(mime ? mime : ""));
  result.set("data", copy_bytes(data, size));
  return result;
}

// Serialises one widget (and its children) to the same shape as web-gphoto2's
// `configToJS`, which the Rust side parses.
val widget_to_js(CameraWidget *widget) {
  val result = val::object();

  const char *name = "";
  const char *label = "";
  const char *info = "";
  int readonly = 0;
  CameraWidgetType type = GP_WIDGET_TEXT;
  gp_widget_get_name(widget, &name);
  gp_widget_get_label(widget, &label);
  gp_widget_get_info(widget, &info);
  gp_widget_get_readonly(widget, &readonly);
  gp_widget_get_type(widget, &type);

  result.set("name", std::string(name ? name : ""));
  result.set("label", std::string(label ? label : ""));
  result.set("info", std::string(info ? info : ""));
  result.set("readonly", readonly != 0);

  switch (type) {
    case GP_WIDGET_RANGE: {
      float value = 0, min = 0, max = 0, step = 0;
      gp_widget_get_value(widget, &value);
      gp_widget_get_range(widget, &min, &max, &step);
      result.set("type", "range");
      result.set("value", value);
      result.set("min", min);
      result.set("max", max);
      result.set("step", step);
      break;
    }
    case GP_WIDGET_MENU:
    case GP_WIDGET_RADIO: {
      const char *value = nullptr;
      gp_widget_get_value(widget, &value);
      result.set("type", type == GP_WIDGET_MENU ? "menu" : "radio");
      result.set("value", std::string(value ? value : ""));
      val choices = val::array();
      for (int i = 0, n = gp_widget_count_choices(widget); i < n; i++) {
        const char *choice = nullptr;
        if (gp_widget_get_choice(widget, i, &choice) == GP_OK && choice) {
          choices.call<void>("push", std::string(choice));
        }
      }
      result.set("choices", choices);
      break;
    }
    case GP_WIDGET_TOGGLE: {
      int value = 0;
      gp_widget_get_value(widget, &value);
      result.set("type", "toggle");
      // Camera actions report 2 here ("unknown"); only 0/1 are real states.
      if (value == 0 || value == 1) result.set("value", value == 1);
      break;
    }
    case GP_WIDGET_TEXT: {
      const char *value = nullptr;
      gp_widget_get_value(widget, &value);
      result.set("type", "text");
      result.set("value", std::string(value ? value : ""));
      break;
    }
    case GP_WIDGET_DATE: {
      int seconds = 0;
      gp_widget_get_value(widget, &seconds);
      result.set("type", "datetime");
      result.set("value", static_cast<double>(seconds) * 1000);
      break;
    }
    case GP_WIDGET_WINDOW:
    case GP_WIDGET_SECTION: {
      result.set("type", type == GP_WIDGET_WINDOW ? "window" : "section");
      val children = val::array();
      for (int i = 0, n = gp_widget_count_children(widget); i < n; i++) {
        CameraWidget *child = nullptr;
        if (gp_widget_get_child(widget, i, &child) == GP_OK && child) {
          children.call<void>("push", widget_to_js(child));
        }
      }
      result.set("children", children);
      break;
    }
    case GP_WIDGET_BUTTON:
    default:
      result.set("type", "button");
      break;
  }
  return result;
}

}  // namespace

// Lists the cameras libgphoto2 can see. Under WebUSB that is only the devices
// the page was granted through `navigator.usb.requestDevice()`.
val list_cameras() {
  gp_ptr<GPContext, gp_context_unref> context(new_context());
  CameraList *raw_list = nullptr;
  int ret = gp_list_new(&raw_list);
  if (ret < GP_OK) return error_value(ret, "gp_list_new");
  gp_ptr<CameraList, gp_list_free> list(raw_list);

  ret = gp_camera_autodetect(list.get(), context.get());
  if (ret < GP_OK) return error_value(ret, "gp_camera_autodetect");

  val cameras = val::array();
  for (int i = 0, n = gp_list_count(list.get()); i < n; i++) {
    const char *model = nullptr;
    const char *port = nullptr;
    gp_list_get_name(list.get(), i, &model);
    gp_list_get_value(list.get(), i, &port);
    // "usb:" alone is the generic placeholder libgphoto2 lists next to the real
    // ports; it does not name a device.
    if (!port || std::strcmp(port, "usb:") == 0) continue;
    val camera = val::object();
    camera.set("model", std::string(model ? model : ""));
    camera.set("port", std::string(port));
    cameras.call<void>("push", camera);
  }
  return cameras;
}

std::string libgphoto2_version() {
  const char **version = gp_library_version(GP_VERSION_SHORT);
  return version && version[0] ? version[0] : "";
}

// One open session with one camera.
class Session {
 public:
  Session(std::string model, std::string port)
      : model_(std::move(model)), port_(std::move(port)) {}

  ~Session() { close(); }

  // Opens the camera on `port`, telling libgphoto2 which driver to use
  // (`model`) instead of letting it probe — the same as the native backend's
  // `Context::get_camera(descriptor)`.
  val open() {
    if (camera_) return val::undefined();
    context_ = new_context();

    Camera *camera = nullptr;
    int ret = gp_camera_new(&camera);
    if (ret < GP_OK) return error_value(ret, "gp_camera_new");

    CameraAbilitiesList *abilities_list = nullptr;
    gp_abilities_list_new(&abilities_list);
    gp_abilities_list_load(abilities_list, context_);
    int index = gp_abilities_list_lookup_model(abilities_list, model_.c_str());
    if (index >= GP_OK) {
      CameraAbilities abilities;
      gp_abilities_list_get_abilities(abilities_list, index, &abilities);
      gp_camera_set_abilities(camera, abilities);
    }
    gp_abilities_list_free(abilities_list);

    GPPortInfoList *port_list = nullptr;
    gp_port_info_list_new(&port_list);
    gp_port_info_list_load(port_list);
    int port_index = gp_port_info_list_lookup_path(port_list, port_.c_str());
    if (port_index >= GP_OK) {
      GPPortInfo info;
      gp_port_info_list_get_info(port_list, port_index, &info);
      gp_camera_set_port_info(camera, info);
    }
    gp_port_info_list_free(port_list);

    ret = gp_camera_init(camera, context_);
    if (ret < GP_OK) {
      gp_camera_unref(camera);
      return error_value(ret, "gp_camera_init");
    }
    camera_ = camera;
    return val::undefined();
  }

  void close() {
    if (camera_) {
      gp_camera_exit(camera_, context_);
      gp_camera_unref(camera_);
      camera_ = nullptr;
    }
    if (context_) {
      gp_context_unref(context_);
      context_ = nullptr;
    }
  }

  bool isOpen() const { return camera_ != nullptr; }

  val config() {
    if (!camera_) return error_message("camera is not open");
    CameraWidget *root = nullptr;
    int ret = gp_camera_get_config(camera_, &root, context_);
    if (ret < GP_OK) return error_value(ret, "gp_camera_get_config");
    val result = widget_to_js(root);
    gp_widget_free(root);
    return result;
  }

  // One widget by config key, or `null` when the body has no such key — how the
  // Rust side probes the several names a parameter is known by.
  val widget(std::string name) {
    if (!camera_) return error_message("camera is not open");
    CameraWidget *widget = nullptr;
    int ret = gp_camera_get_single_config(camera_, name.c_str(), &widget, context_);
    if (ret < GP_OK) return val::null();
    val result = widget_to_js(widget);
    gp_widget_free(widget);
    return result;
  }

  // Sets a widget from its string form: a choice for radios/menus, a number for
  // ranges, "true"/"false" (or 1/0) for toggles.
  val setConfig(std::string name, std::string value) {
    if (!camera_) return error_message("camera is not open");
    CameraWidget *widget = nullptr;
    int ret = gp_camera_get_single_config(camera_, name.c_str(), &widget, context_);
    if (ret < GP_OK) return error_value(ret, "gp_camera_get_single_config(" + name + ")");

    CameraWidgetType type = GP_WIDGET_TEXT;
    gp_widget_get_type(widget, &type);
    switch (type) {
      case GP_WIDGET_RANGE: {
        float number = std::strtof(value.c_str(), nullptr);
        ret = gp_widget_set_value(widget, &number);
        break;
      }
      case GP_WIDGET_MENU:
      case GP_WIDGET_RADIO:
      case GP_WIDGET_TEXT:
        ret = gp_widget_set_value(widget, value.c_str());
        break;
      case GP_WIDGET_TOGGLE: {
        int on = (value == "true" || value == "1" || value == "on") ? 1 : 0;
        ret = gp_widget_set_value(widget, &on);
        break;
      }
      default:
        gp_widget_free(widget);
        return error_message("widget '" + name + "' cannot be set");
    }
    if (ret < GP_OK) {
      gp_widget_free(widget);
      return error_value(ret, "gp_widget_set_value(" + name + ")");
    }

    ret = gp_camera_set_single_config(camera_, name.c_str(), widget, context_);
    gp_widget_free(widget);
    if (ret < GP_OK) return error_value(ret, "gp_camera_set_single_config(" + name + ")");
    return val::undefined();
  }

  // A live-view frame: no shutter actuation, no file on the card.
  val capturePreview() {
    if (!camera_) return error_message("camera is not open");
    CameraFile *raw = nullptr;
    gp_file_new(&raw);
    gp_ptr<CameraFile, gp_file_unref> file(raw);
    int ret = gp_camera_capture_preview(camera_, file.get(), context_);
    if (ret < GP_OK) return error_value(ret, "gp_camera_capture_preview");
    return file_to_js(file.get());
  }

  // A full still, downloaded into memory. The format follows the body's image
  // quality setting, like the native backend.
  val captureImage() {
    if (!camera_) return error_message("camera is not open");
    CameraFilePath path;
    std::memset(&path, 0, sizeof(path));
    int ret = gp_camera_capture(camera_, GP_CAPTURE_IMAGE, &path, context_);
    if (ret < GP_OK) return error_value(ret, "gp_camera_capture");

    CameraFile *raw = nullptr;
    gp_file_new(&raw);
    gp_ptr<CameraFile, gp_file_unref> file(raw);
    ret = gp_camera_file_get(camera_, path.folder, path.name, GP_FILE_TYPE_NORMAL,
                             file.get(), context_);
    if (ret < GP_OK) return error_value(ret, "gp_camera_file_get");

    val result = file_to_js(file.get());
    result.set("name", std::string(path.name));
    return result;
  }

  // Drains pending camera events. Some bodies queue an event per capture or
  // property change and stall once the queue is full, so the bridge calls this
  // between operations. Returns whether any event was pending.
  bool consumeEvents() {
    if (!camera_) return false;
    bool had_events = false;
    for (int i = 0; i < 64; i++) {
      CameraEventType type = GP_EVENT_UNKNOWN;
      void *data = nullptr;
      int ret = gp_camera_wait_for_event(camera_, 0, &type, &data, context_);
      std::free(data);
      if (ret < GP_OK || type == GP_EVENT_TIMEOUT) break;
      had_events = true;
    }
    return had_events;
  }

 private:
  std::string model_;
  std::string port_;
  Camera *camera_ = nullptr;
  GPContext *context_ = nullptr;
};

// Every call that may reach the camera can suspend (libusb's WebUSB backend
// awaits the browser's promises through Asyncify), so it is bound with the
// `async` policy and returns a Promise on the JS side.
EMSCRIPTEN_BINDINGS(toucan_gphoto2) {
  using emscripten::async;
  emscripten::function("listCameras", &list_cameras, async());
  emscripten::function("libgphoto2Version", &libgphoto2_version);
  emscripten::class_<Session>("Session")
      .constructor<std::string, std::string>()
      .function("open", &Session::open, async())
      .function("close", &Session::close, async())
      .function("isOpen", &Session::isOpen)
      .function("config", &Session::config, async())
      .function("widget", &Session::widget, async())
      .function("setConfig", &Session::setConfig, async())
      .function("capturePreview", &Session::capturePreview, async())
      .function("captureImage", &Session::captureImage, async())
      .function("consumeEvents", &Session::consumeEvents, async());
}
