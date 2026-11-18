# Builds libgphoto2 (ptp2 camlib only) + libusb (WebUSB backend) + libltdl to
# WebAssembly with Emscripten, then links them with toucan_gphoto2.cpp into an
# ES module: out/toucan_gphoto2.{mjs,wasm}.
#
# Run inside the Emscripten image through ./build.sh, never directly on the host
# (every compiler here must be emcc).
#
# libgphoto2 is upstream, unmodified: no fork and no patch. What web-gphoto2's
# patch changed in it (plugin discovery, threads) is handled by the build flags
# below and by toucan_gphoto2.cpp (`ensure_plugins`).

# The same libgphoto2 as the native release builds: the ref is read from the CI
# workflow, its single source of truth. Override on the command line to try
# another one (make LIBGPHOTO2_REF=...).
WORKFLOW := ../../.github/workflows/build.yml
LIBGPHOTO2_REF ?= $(shell sed -n 's/^  LIBGPHOTO2_REF: "\(.*\)"/\1/p' $(WORKFLOW))
ifeq ($(strip $(LIBGPHOTO2_REF)),)
$(error LIBGPHOTO2_REF not found in $(WORKFLOW))
endif
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

# -pthread everywhere: libusb's WebUSB backend waits for transfer completion
# with Atomics.waitAsync on the wasm heap, which needs shared memory — even
# single-threaded (libusb 1.0.27 through 1.0.29). Wasm objects with and without
# shared memory cannot be linked together, so every library gets the flag (a
# build flag is all libgphoto2 needs; web-gphoto2 patched its configure.ac for
# this). Consequence: SharedArrayBuffer, hence COOP/COEP on the page.
COMMON_FLAGS = -Os -flto -pthread
export CFLAGS += $(COMMON_FLAGS)
export CXXFLAGS += $(COMMON_FLAGS)
export LDFLAGS += $(COMMON_FLAGS)

all: $(OUT)/toucan_gphoto2.mjs

## Final module

# The ptp2 camlib and usb1 iolib are plain static archives here; the table that
# lets libgphoto2 find them by name is in toucan_gphoto2.cpp (`kPreloaded`).
$(OUT)/toucan_gphoto2.mjs: $(WORK)/toucan_gphoto2.o $(SYSROOT)/lib/libltdl.la $(SYSROOT)/lib/libgphoto2.la
	mkdir -p $(OUT)
	set -eu; \
	PTP2_A=$$(find $(SYSROOT)/lib/libgphoto2 -name libptp2.a -print -quit); \
	USB1_A=$$(find $(SYSROOT)/lib/libgphoto2_port -name libusb1.a -print -quit); \
	: $${PTP2_A:?missing libgphoto2 camlib libptp2.a}; \
	: $${USB1_A:?missing libgphoto2_port iolib libusb1.a}; \
	$(WORK)/libgphoto2/libtool --tag=CXX --mode=link $(CXX) $(LDFLAGS) -o $@ $(WORK)/toucan_gphoto2.o \
		$$PTP2_A $$USB1_A $(SYSROOT)/lib/libusb-1.0.la \
		$(SYSROOT)/lib/libgphoto2.la $(SYSROOT)/lib/libltdl.la \
		--bind -pthread -s ASYNCIFY -s ALLOW_MEMORY_GROWTH -s ENVIRONMENT=web,worker \
		-s MODULARIZE -s EXPORT_ES6 -s EXPORT_NAME=initToucanGPhoto2

$(WORK)/toucan_gphoto2.o: toucan_gphoto2.cpp $(SYSROOT)/lib/libgphoto2.la
	$(CXX) $(CPPFLAGS) $(CXXFLAGS) -std=c++17 -c $< -o $@

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
	curl -fsSL -o $(WORK)/libusb.tar https://github.com/libusb/libusb/releases/download/v$(LIBUSB_VERSION)/libusb-$(LIBUSB_VERSION).tar.bz2
	tar xf $(WORK)/libusb.tar --strip 1 -C $(@D)

$(WORK)/libusb/Makefile: $(WORK)/libusb/configure | $(SYSROOT)
	cd $(@D) && ./configure --prefix=$(SYSROOT) --disable-shared --host=wasm32-emscripten

$(SYSROOT)/lib/libusb-1.0.la: $(WORK)/libusb/Makefile
	$(MAKE) -C $(WORK)/libusb install

## libgphoto2

# A checkout per ref: changing LIBGPHOTO2_REF rebuilds libgphoto2 from scratch.
LIBGPHOTO2_SRC := $(WORK)/libgphoto2

$(LIBGPHOTO2_SRC)/.ref-$(LIBGPHOTO2_REF):
	mkdir -p $(LIBGPHOTO2_SRC)
	test -d $(LIBGPHOTO2_SRC)/.git || git -C $(LIBGPHOTO2_SRC) init -q
	git -C $(LIBGPHOTO2_SRC) remote add origin $(LIBGPHOTO2_REPO) 2>/dev/null || true
	git -C $(LIBGPHOTO2_SRC) fetch -q --depth 1 origin $(LIBGPHOTO2_REF)
	git -C $(LIBGPHOTO2_SRC) checkout -q -f FETCH_HEAD
	git -C $(LIBGPHOTO2_SRC) clean -qfdx
	rm -f $(SYSROOT)/lib/libgphoto2.la
	touch $@

$(WORK)/libgphoto2/configure: $(LIBGPHOTO2_SRC)/.ref-$(LIBGPHOTO2_REF) | $(SYSROOT)/lib/libltdl.la
	cd $(@D) && autoreconf -fi

$(WORK)/libgphoto2/Makefile: $(WORK)/libgphoto2/configure $(SYSROOT)/lib/libusb-1.0.la $(SYSROOT)/lib/libltdl.la
	cd $(@D) && ./configure --prefix=$(SYSROOT) --disable-shared --host=wasm32-emscripten \
		--without-libxml-2.0 --without-libexif --without-libcurl --without-gdlib --without-jpeg \
		--disable-nls --disable-ptpip --disable-disk --with-camlibs=ptp2

$(SYSROOT)/lib/libgphoto2.la: $(WORK)/libgphoto2/Makefile
	$(MAKE) -C $(WORK)/libgphoto2 install

.PHONY: all
