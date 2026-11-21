#!/usr/bin/env node
import fs from "node:fs";
import path from "node:path";

const rootDir = path.resolve(import.meta.dirname, "..");
const androidAppDir = path.join(rootDir, "src-tauri", "gen", "android", "app");
const kotlinTargetDir = path.join(androidAppDir, "src", "main", "java", "dev", "lukeshare", "stream");
const manifestPath = path.join(androidAppDir, "src", "main", "AndroidManifest.xml");
const buildGradlePath = path.join(androidAppDir, "build.gradle.kts");

if (!fs.existsSync(androidAppDir)) {
  throw new Error(`Diretório Android não encontrado em ${androidAppDir}. Execute 'npx tauri android init' antes.`);
}

// 1. Copia os arquivos Kotlin nativos do Meta Quest 3S (sobrescrevendo o MainActivity.kt padrão)
fs.mkdirSync(kotlinTargetDir, { recursive: true });
for (const file of ["MainActivity.kt", "QuestCaptureService.kt", "QuestSingleEyeRenderer.kt"]) {
  const src = path.join(import.meta.dirname, file);
  const dst = path.join(kotlinTargetDir, file);
  fs.copyFileSync(src, dst);
  console.log(`[quest-patch] Copiado ${file} -> ${path.relative(rootDir, dst)}`);
}

// 2. Garante minSdk = 29 (Android 10+ exigido para AudioPlaybackCapture e FOREGROUND_SERVICE_TYPE_MEDIA_PROJECTION)
if (fs.existsSync(buildGradlePath)) {
  let gradle = fs.readFileSync(buildGradlePath, "utf8");
  if (/minSdk\s*=\s*\d+/.test(gradle)) {
    gradle = gradle.replace(/minSdk\s*=\s*\d+/, "minSdk = 29");
  } else if (/minSdkVersion\s*\(\s*\d+\s*\)/.test(gradle)) {
    gradle = gradle.replace(/minSdkVersion\s*\(\s*\d+\s*\)/, "minSdkVersion(29)");
  } else if (/defaultConfig\s*\{/.test(gradle)) {
    gradle = gradle.replace(/defaultConfig\s*\{/, "defaultConfig {\n        minSdk = 29");
  }
  fs.writeFileSync(buildGradlePath, gradle, "utf8");
  console.log("[quest-patch] minSdk = 29 configurado em app/build.gradle.kts");
}

// 3. Injeta permissões, suporte Meta Quest (Horizon OS) e declaração do QuestCaptureService no AndroidManifest.xml
if (!fs.existsSync(manifestPath)) {
  throw new Error(`AndroidManifest.xml não encontrado em ${manifestPath}`);
}

let manifest = fs.readFileSync(manifestPath, "utf8");

const permissionsToAdd = [
  "android.permission.INTERNET",
  "android.permission.FOREGROUND_SERVICE",
  "android.permission.FOREGROUND_SERVICE_MEDIA_PROJECTION",
  "android.permission.RECORD_AUDIO",
  "android.permission.WAKE_LOCK",
  "android.permission.ACCESS_WIFI_STATE",
  "android.permission.CHANGE_WIFI_STATE",
];

let permBlock = "";
for (const perm of permissionsToAdd) {
  if (!manifest.includes(perm)) {
    permBlock += `    <uses-permission android:name="${perm}" />\n`;
  }
}

if (!manifest.includes("android.hardware.vr.headtracking")) {
  permBlock += `    <uses-feature android:name="android.hardware.vr.headtracking" android:required="false" />\n`;
}

if (permBlock.length > 0) {
  manifest = manifest.replace(/<application\b/, `${permBlock}\n    <application`);
}

let appExtras = "";
if (!manifest.includes("com.oculus.supportedDevices")) {
  appExtras += `        <meta-data android:name="com.oculus.supportedDevices" android:value="quest2|questpro|quest3|quest3s" />\n`;
}
if (!manifest.includes("QuestCaptureService")) {
  appExtras += `        <service\n            android:name=".QuestCaptureService"\n            android:exported="false"\n            android:foregroundServiceType="mediaProjection" />\n`;
}

if (appExtras.length > 0) {
  manifest = manifest.replace(/<\/application>/, `${appExtras}    </application>`);
}

fs.writeFileSync(manifestPath, manifest, "utf8");
console.log("[quest-patch] AndroidManifest.xml atualizado para Meta Quest 3S.");
