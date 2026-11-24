# Atman for iOS and Android

Open a paired Session with its existing Flow directory, conversation, Console and approval queue. Both apps bundle the Svelte workbench and stream HTTPS updates. iOS stores its device credential record in Keychain. Android seals it with an Android Keystore key in app-private storage.

## Build for iOS

The iOS target requires iOS 17.4 or later. Use Xcode with an installed iOS Simulator runtime. From the TypeScript workspace, build the bundle and synchronize the Xcode project.

```sh
bun install --frozen-lockfile
bun --filter @atman/mobile build
bun --filter @atman/mobile cap sync ios
bun --filter @atman/mobile cap open ios
```

## Build for Android

Use JDK 21 and Android SDK Platform 36. The app supports Android 7.0 and later with Android System WebView 116 or later. From the TypeScript workspace, build and sync the bundle before invoking the Gradle wrapper.

```sh
bun install --frozen-lockfile
bun --filter @atman/mobile build
bun --filter @atman/mobile cap sync android
cd mobile/android
./gradlew :app:assembleDebug
```

## Pair

Use an invitation from the host's HTTPS device listener. Paste the complete link into **Pair this device**, or use **Scan QR code** with a QR code containing that link. Verify the displayed origin and select **Pair device**. The device must trust the host certificate. A device's grant selects the Session and which actions appear in the workbench. [Host setup](../../../docs/remote-access.md)
