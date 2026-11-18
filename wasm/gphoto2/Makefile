# Builds libgphoto2 (ptp2 camlib only) + libusb (WebUSB backend) + libltdl to
# WebAssembly with Emscripten, then links them with toucan_gphoto2.cpp into an
# ES module: out/toucan_gphoto2.{mjs,wasm}.
#
# Run inside the Emscripten image through ./build.sh, never directly on the host
# (every compiler here must be emcc). Adapted from web-gphoto2's Makefile.

LIBGPHOTO2_REF ?= 7fc007d0ba6b52b8608da7b46920ae2416a94358
LIBGPHOTO2_REPO ?= https://github.com/gphoto/libgphoto2.git
LIBUSB_VERSION ?= 1.0.27
LIBTOOL_VERSION ?= 2.4.6
# Any mirror of the release tarball works (gz or xz: tar detects the format),
# e.g. http://archive.ubuntu.com/ubuntu/pool/main/libt/libtool/libtool_2.4.6.orig.tar.xz
LIBTOOL_URL ?= https://ftpmirror.gnu.org/libtool/libtool-$(LIBTOOL_VERSION).tar.gz

WORK := work
SYSROOT := $(abspath $(WORK)/sysroot)
OUT := out

export PKG_CONFIG_PATH := $(SYSROOT)/lib/pkgconfig
export PKG_CONFIG_LIBDIR := $(SYSROOT)/lib/pkgconfig
export CPPFLAGS += -I$(SYSROOT)/include
export LDFLAGS += -L$(SYSROOT)/lib -s DYNAMIC_EXECUTION=0 -s AUTO_JS_LIBRARIES=0 -s AUTO_NATIVE_LIBRARIES=0

COMMON_FLAGS = -Os -flto
export CFLAGS += $(COMMON_FLAGS)
export CXXFLAGS += $(COMMON_FLAGS)
export LDFLAGS += $(COMMON_FLAGS)

all: $(OUT)/toucan_gphoto2.mjs

## Final module

# -pthread: libgphoto2_port is built with it (its configure finds pthread_create),
# and wasm objects with and without shared memory cannot be mixed. The page must
# therefore be cross-origin isolated (COOP/COEP) for SharedArrayBuffer.
$(OUT)/toucan_gphoto2.mjs: $(WORK)/toucan_gphoto2.o $(SYSROOT)/lib/libltdl.la $(SYSROOT)/lib/libgphoto2.la
	mkdir -p $(OUT)
	set -eu; \
	PTP2_LA=$$(find $(SYSROOT)/lib/libgphoto2 -name ptp2.la -print -quit); \
	USB1_LA=$$(find $(SYSROOT)/lib/libgphoto2_port -name usb1.la -print -quit); \
	: $${PTP2_LA:?missing libgphoto2 ptp2.la}; \
	: $${USB1_LA:?missing libgphoto2_port usb1.la}; \
	$(WORK)/libgphoto2/libtool --tag=CXX --mode=link $(CXX) $(LDFLAGS) -o $@ $(WORK)/toucan_gphoto2.o \
		$(SYSROOT)/lib/libltdl.la $(SYSROOT)/lib/libgphoto2.la \
		--bind -pthread -s ASYNCIFY -s ALLOW_MEMORY_GROWTH -s ENVIRONMENT=web,worker \
		-s MODULARIZE -s EXPORT_ES6 -s EXPORT_NAME=initToucanGPhoto2 \
		-dlpreopen $$PTP2_LA \
		-dlpreopen $$USB1_LA

$(WORK)/toucan_gphoto2.o: toucan_gphoto2.cpp $(SYSROOT)/lib/libgphoto2.la
	$(CXX) $(CPPFLAGS) $(CXXFLAGS) -std=c++17 -pthread -c $< -o $@

$(SYSROOT):
	mkdir -p $@

## libtool (libltdl)

$(WORK)/libtool/configure:
	mkdir -p $(@D)
	curl -fsSL -o $(WORK)/libtool.tar $(LIBTOOL_URL)
	tar xf $(WORK)/libtool.tar --strip 1 -C $(@D)

$(WORK)/libtool/Makefile: $(WORK)/libtool/configure | $(SYSROOT)
	cd $(@D) && ./configure --prefix=$(SYSROOT) --disable-shared

$(SYSROOT)/lib/libltdl.la: $(WORK)/libtool/Makefile
	$(MAKE) -C $(WORK)/libtool install

## libusb (WebUSB backend)

$(WORK)/libusb/configure:
	mkdir -p $(@D)
	curl -fsSL https://github.com/libusb/libusb/releases/download/v$(LIBUSB_VERSION)/libusb-$(LIBUSB_VERSION).tar.bz2 | tar x --strip 1 -C $(@D)

$(WORK)/libusb/Makefile: $(WORK)/libusb/configure | $(SYSROOT)
	cd $(@D) && ./configure --prefix=$(SYSROOT) --disable-shared --host=wasm32-emscripten

$(SYSROOT)/lib/libusb-1.0.la: $(WORK)/libusb/Makefile
	$(MAKE) -C $(WORK)/libusb install

## libgphoto2

$(WORK)/libgphoto2/.patched: patches/libgphoto2-emscripten.patch
	mkdir -p $(WORK)/libgphoto2
	test -d $(WORK)/libgphoto2/.git || git -C $(WORK)/libgphoto2 init -q
	git -C $(WORK)/libgphoto2 remote add origin $(LIBGPHOTO2_REPO) 2>/dev/null || true
	git -C $(WORK)/libgphoto2 fetch -q --depth 1 origin $(LIBGPHOTO2_REF)
	git -C $(WORK)/libgphoto2 checkout -q -f FETCH_HEAD
	git -C $(WORK)/libgphoto2 clean -qfdx
	cd $(WORK)/libgphoto2 && patch -p1 --forward < $(abspath patches/libgphoto2-emscripten.patch)
	touch $@

$(WORK)/libgphoto2/configure: $(WORK)/libgphoto2/.patched | $(SYSROOT)/lib/libltdl.la
	cd $(@D) && autoreconf -fi

$(WORK)/libgphoto2/Makefile: $(WORK)/libgphoto2/configure $(SYSROOT)/lib/libusb-1.0.la $(SYSROOT)/lib/libltdl.la
	cd $(@D) && ./configure --prefix=$(SYSROOT) --disable-shared --host=wasm32-emscripten \
		--without-libxml-2.0 --without-libexif --without-libcurl --without-gdlib --without-jpeg \
		--disable-nls --disable-ptpip --disable-disk --with-camlibs=ptp2

$(SYSROOT)/lib/libgphoto2.la: $(WORK)/libgphoto2/Makefile
	$(MAKE) -C $(WORK)/libgphoto2 install

.PHONY: all
