# Keep the JNI bridge class and every native entry point intact under R8.
# The Rust side resolves these by the fully-qualified
# Java_com_audioshare_ultra_service_AudioEngineService_native* symbol name.
-keep class com.audioshare.ultra.service.AudioEngineService {
    native <methods>;
}
-keepclassmembers class * { native <methods>; }
