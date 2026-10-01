android {
    compileSdk = 35
    defaultConfig {
        targetSdk = 35
        minSdk = 24 // Supported devices must stay supported
    }
}
dependencies {
    implementation("androidx.webkit:webkit:1.6.1")
    implementation("androidx.webkit:webkit:$webkitVersion")
    implementation("androidx.appcompat:appcompat:1.6.1")
    implementation("com.google.android.material:material:1.8.0")
    testImplementation("junit:junit:4.13.2")
    androidTestImplementation("androidx.test.ext:junit:1.1.4")
    androidTestImplementation("androidx.test.espresso:espresso-core:3.5.0")
    implementation("dynamic.example:library:${lookupVersion()}")
}
