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

For APNs, enable Push Notifications on the signed App ID and set `ATMAN_PUSH_ENTITLEMENTS=App/Push.entitlements` in the App build settings. Set `ATMAN_APNS_ENVIRONMENT=development` for the APNs sandbox or `production` for production. The daemon's APNs environment and topic must match the signed App.

## Build for Android

Use JDK 21 and Android SDK Platform 36. The app supports Android 7.0 and later with Android System WebView 116 or later. From the TypeScript workspace, build and sync the bundle before invoking the Gradle wrapper.

```sh
bun install --frozen-lockfile
bun --filter @atman/mobile build
bun --filter @atman/mobile cap sync android
cd mobile/android
./gradlew :app:assembleDebug
```

For FCM, put the Firebase project's `google-services.json` in `mobile/android/app/` before building. Register the installed package ID in that Firebase project. Configure the daemon's FCM sender or the relay to use the same Firebase project.

## Pair

Use an invitation from the host's HTTPS device listener. Paste the complete link into **Pair this device**, or use **Scan QR code** with a QR code containing that link. Verify the displayed origin and select **Pair device**. The device must trust the host certificate. A device's grant selects the Session and which actions appear in the workbench. [Host setup](../../../docs/remote-access.md)

If the host is temporarily unreachable at startup, keep the app open to reconnect to the saved Session automatically, or select **Reconnect** to try immediately. Typing or scanning an invitation pauses automatic recovery so the pairing form stays available. A revoked grant requires a fresh invitation.

The native HTTPS bridge binds subscription requests to the paired host and the platform's push endpoint. Relay management uses a separate native entry point bound to the approved relay origin, registration ID, and owner key in the paired secure record.

## Notifications

Open **Notifications** in the paired workbench header. Select direct delivery or the displayed HTTPS relay, then select **Turn on** to grant system permission and register this installation. **Turn off** waits for confirmation from the host and relay. Signing out clears the device record after delivery is disabled.
