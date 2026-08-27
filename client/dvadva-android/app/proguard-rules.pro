# Keep the wire/bridge protocol layer intact if minification is ever enabled:
# it is all plain Kotlin and kotlinx.serialization over JsonElement, which does
# not need rules, but the JSON field names are a wire contract — never let a
# shrinker touch them.
-keep class dev.dvadva.android.proto.** { *; }
