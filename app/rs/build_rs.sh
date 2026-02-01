#! /bin/bash

#
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
#

export ANDROID_NDK_HOME="C:/Users/faris/AppData/Local/Android/Sdk/ndk/22.1.7171670"
export RUSTFLAGS="-C link-arg=-lc++_shared -C link-arg=-lm -C link-arg=-ldl -C link-arg=-z -C link-arg=max-page-size=16384"

cargo xdk -t arm64-v8a -o ../src/main/jniLibs build $1
