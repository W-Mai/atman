# Atman for iOS

Open a paired Session with its existing Flow directory, conversation, Console and approval queue. The iOS app bundles the Svelte workbench, uses Keychain for its device credential record, and streams HTTPS updates through `URLSession`.

## Build

The iOS target requires iOS 17.4 or later. Use Xcode with an installed iOS Simulator runtime. From the TypeScript workspace, build the bundle and synchronize the Xcode project.

```sh
bun install --frozen-lockfile
bun --filter @atman/mobile build
bun --filter @atman/mobile cap sync ios
bun --filter @atman/mobile cap open ios
```

## Pair

Use an invitation from the host's HTTPS device listener. Paste the complete link into **Pair this device**, or use **Scan QR code** with a QR code containing that link. Verify the displayed origin and select **Pair device**. The host certificate must be trusted by iOS. A device's grant selects the Session and which actions appear in the workbench. [Host setup](../../../docs/remote-access.md)
