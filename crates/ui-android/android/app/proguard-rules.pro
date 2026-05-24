# FrenchTV ProGuard rules
# The Rust .so contains no JNI symbols that need to be preserved here
# (all JNI calls go from Rust → Java, not the other way).
-keep class com.frenchetv.PlayerActivity { *; }
