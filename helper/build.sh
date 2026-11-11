#!/bin/sh
# Builds helper/phantomqa-helper.jar from Helper.java. The jar is committed and built into the
# phantomqa binary, so only a change to Helper.java needs this.
#
# Needs: a JDK (javac) and the Android SDK with build-tools and one platform.
#   ANDROID_HOME=/path/to/sdk helper/build.sh
set -eu
cd "$(dirname "$0")"

sdk=${ANDROID_HOME:-${ANDROID_SDK_ROOT:-}}
if [ -z "$sdk" ]; then
    echo "set ANDROID_HOME to the Android SDK" >&2
    exit 1
fi
platform=$(ls -d "$sdk"/platforms/android-* | sort -V | tail -n 1)
tools=$(ls -d "$sdk"/build-tools/* | sort -V | tail -n 1)

rm -rf build
mkdir build
# Java 8 class files: d8 turns them into code for Android 5 (API 21) and later.
javac -Xlint:-options -source 8 -target 8 -bootclasspath "$platform/android.jar" -d build Helper.java
"$tools/d8" --min-api 21 --lib "$platform/android.jar" --output phantomqa-helper.jar build/phantomqa/*.class
rm -rf build
echo "built helper/phantomqa-helper.jar with $(basename "$platform") and build-tools $(basename "$tools")"
