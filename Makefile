#
# Copyright 2019-2021 Signal Messenger, LLC
# SPDX-License-Identifier: AGPL-3.0-only
#

V ?= 0
Q = @
ifneq ($V,0)
	Q =
endif

PREPARE_WORKSPACE ?= 0

USE_PREBUILD ?= 0
BUILD_WHAT := $(BUILD_WHAT)
ifeq ($(USE_PREBUILD), 1)
	BUILD_WHAT = "ringrtc"
endif

BUILD_TYPES := release debug

GN_ARCHS     := arm arm64 x86 x64

# This can be overridden on the command line, e.g. "make electron NODEJS_ARCH=ia32"
# Note: make sure to only use NodeJS architectures here, like x64, ia32, arm64, etc.
NODEJS_ARCH := x64

help:
	$(Q) echo "The following build targets are supported:"
	$(Q) echo "  ios          -- build for the iOS platform"
	$(Q) echo "  android      -- build for the Android platform"
	$(Q) echo "  electron     -- build an Electron library"
	$(Q) echo "  direct       -- build the direct/1:1 call test cli"
	$(Q) echo "  gctc         -- build the group call test cli"
	$(Q) echo "  call_sim-cli -- build the call simulator test cli"
	$(Q) echo
	$(Q) echo "For release builds, you can also set USE_PREBUILD=1 to download "
	$(Q) echo "a \"prebuild\" of WebRTC and use that instead of downloading and "
	$(Q) echo "building WebRTC. For example:"
	$(Q) echo "  $ TYPE=release make electron NODEJS_ARCH=arm64 USE_PREBUILD=1"
	$(Q) echo "  $ make ios USE_PREBUILD=1"
	$(Q) echo
	$(Q) echo "Specify PREPARE_WORKSPACE=1 to request a sync of WebRTC code."
	$(Q) echo "For example:"
	$(Q) echo "  $ make ios PREPARE_WORKSPACE=1"
	$(Q) echo
	$(Q) echo "For the electron/cli/gctc builds, you may also specify a different"
	$(Q) echo "platform for which to download WebRTC. For example:"
	$(Q) echo "  $ make electron PLATFORM=unix PREPARE_WORKSPACE=1"
	$(Q) echo
	$(Q) echo "PREPARE_WORKSPACE=1 is mutually exclusive with USE_PREBUILD=1."
	$(Q) echo
	$(Q) echo "The following clean targets are supported:"
	$(Q) echo "  clean     -- remove all build artifacts"
	$(Q) echo "  distclean -- remove everything"
	$(Q) echo

ifeq ($(PREPARE_WORKSPACE), 1)
android: prepare_workspace
ios: prepare_workspace
electron: prepare_workspace
cli: prepare_workspace
gctc: prepare_workspace
call_sim-cli: prepare_workspace
else ifeq ($(USE_PREBUILD), 1)
android: fetch_artifact
ios: fetch_artifact
electron: fetch_artifact
cli: fetch_artifact
gctc: fetch_artifact
call_sim-cli: fetch_artifact
endif

android: PLATFORM := android
android:
	$(Q) if [ "$(TYPE)" = "debug" ] ; then \
		echo "Android: debug build"; \
		BUILD_WHAT="$(BUILD_WHAT)" ./bin/build-aar --debug; \
	else \
		echo "Android: Release build"; \
		BUILD_WHAT="$(BUILD_WHAT)" ./bin/build-aar --release; \
	fi

ios: PLATFORM := ios
ios:
	$(Q) if [ "$(TYPE)" = "debug" ] ; then \
		echo "iOS: Debug build" ; \
		BUILD_WHAT="$(BUILD_WHAT)" ./bin/build-ios -d ; \
	else \
		echo "iOS: Release build" ; \
		BUILD_WHAT="$(BUILD_WHAT)" ./bin/build-ios ; \
	fi

electron: PLATFORM ?= desktop
electron:
	$(Q) if [ "$(TYPE)" = "debug" ] ; then \
		echo "Electron: Debug build" ; \
		TARGET_ARCH=$(NODEJS_ARCH) BUILD_WHAT=$(BUILD_WHAT) \
			BUILD_WEBRTC_TESTS=$(BUILD_WEBRTC_TESTS) ./bin/build-desktop -d --no-call-sim-cli ; \
	else \
		echo "Electron: Release build" ; \
		TARGET_ARCH=$(NODEJS_ARCH) BUILD_WHAT=$(BUILD_WHAT) \
			BUILD_WEBRTC_TESTS=$(BUILD_WEBRTC_TESTS) ./bin/build-desktop -r --no-call-sim-cli ; \
	fi
	$(Q) (cd src/node && npm install && npm run build)

cli: PLATFORM ?= desktop
cli:
	$(Q) if [ "$(TYPE)" = "release" ] ; then \
		echo "cli: Release build" ; \
		BUILD_WHAT=$(BUILD_WHAT) ./bin/build-direct -r ; \
	else \
		echo "cli: Debug build" ; \
		BUILD_WHAT=$(BUILD_WHAT) ./bin/build-direct -d ; \
	fi

gctc: PLATFORM ?= desktop
gctc:
	$(Q) if [ "$(TYPE)" = "release" ] ; then \
		echo "gctc: Release build" ; \
		BUILD_WHAT=$(BUILD_WHAT) ./bin/build-gctc -r ; \
	else \
		echo "gctc: Debug build" ; \
		BUILD_WHAT=$(BUILD_WHAT) ./bin/build-gctc -d ; \
	fi

call_sim-cli: PLATFORM ?= desktop
call_sim-cli:
	$(Q) if [ "$(TYPE)" = "debug" ] ; then \
		echo "call_sim-cli: Debug build" ; \
		BUILD_WHAT=$(BUILD_WHAT) ./bin/build-desktop --no-electron -d ; \
	else \
		echo "call_sim-cli: Release build" ; \
		BUILD_WHAT=$(BUILD_WHAT) ./bin/build-desktop --no-electron -r ; \
	fi

PHONY += clean
clean:
	$(Q) ./bin/build-aar --clean
	$(Q) ./bin/build-ios --clean
	$(Q) ./bin/build-desktop --clean
	$(Q) ./bin/build-direct --clean
	$(Q) ./bin/build-gctc --clean
	$(Q) rm -rf ./src/webrtc/src/out

PHONY += distclean
distclean:
	$(Q) rm -rf ./out
	$(Q) rm -rf ./target
	$(Q) rm -rf ./src/node/build
	$(Q) rm -rf ./src/node/dist
	$(Q) rm -rf ./src/node/node_modules
	$(Q) rm -rf ./src/webrtc/src/out

PHONY += prepare_workspace
prepare_workspace:
	$(Q) ./bin/prepare-workspace "$(PLATFORM)"

PHONY += fetch_artifact
fetch_artifact:
	$(Q) ./bin/fetch-artifact -p "$(PLATFORM)"

.PHONY: $(PHONY)
