#!/usr/bin/env bash
# Builds build/compose-ime.apk with the SDK's build tools; no Gradle.
set -euo pipefail
SDK=${ANDROID_HOME:-$HOME/Android/Sdk}
BT=$SDK/build-tools/35.0.0
JAR=$SDK/platforms/android-35/android.jar
JDK=${JDK:-/usr/lib/jvm/java-17-openjdk}
cd "$(dirname "$0")"
OUT=build
rm -rf "$OUT/tmp" && mkdir -p "$OUT/tmp/classes"
"$BT/aapt2" compile --dir res -o "$OUT/tmp/res.zip"
"$BT/aapt2" link -I "$JAR" --manifest AndroidManifest.xml -o "$OUT/tmp/unsigned.apk" "$OUT/tmp/res.zip"
"$JDK/bin/javac" --release 17 -classpath "$JAR" -d "$OUT/tmp/classes" $(find src -name '*.java')
"$BT/d8" --lib "$JAR" --min-api 33 --output "$OUT/tmp" $(find "$OUT/tmp/classes" -name '*.class')
(cd "$OUT/tmp" && zip -q unsigned.apk classes.dex)
"$BT/zipalign" -f 4 "$OUT/tmp/unsigned.apk" "$OUT/tmp/aligned.apk"
# A keystore of its own, kept across builds so reinstalls keep the same signature.
if [ ! -f "$OUT/compose-ime.jks" ]; then
  "$JDK/bin/keytool" -genkeypair -keystore "$OUT/compose-ime.jks" -storepass compose -keypass compose \
    -alias compose -keyalg RSA -keysize 2048 -validity 10000 -dname CN=compose-ime >/dev/null 2>&1
fi
PATH="$JDK/bin:$PATH" "$BT/apksigner" sign --ks "$OUT/compose-ime.jks" --ks-pass pass:compose \
  --out "$OUT/compose-ime.apk" "$OUT/tmp/aligned.apk"
rm -rf "$OUT/tmp"
echo "$OUT/compose-ime.apk"
