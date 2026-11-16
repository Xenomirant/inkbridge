# JNA and the UniFFI-generated bindings are called reflectively from native code.
-keep class com.sun.jna.** { *; }
-keep class * implements com.sun.jna.** { *; }
-keep class org.inkbridge.core.** { *; }
-dontwarn java.awt.**
