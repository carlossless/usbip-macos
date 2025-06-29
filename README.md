# usbip-macos

A [USB/IP](https://usbip.sourceforge.net/) client for macOS.

## Disclaimer

This is highly experimental and currently lacks various features for a full USB/IP implementation. It's has just enough working to work my current usecases. Use at your own risk.

## Permissions

Because this tool uses `IOUSBHostControllerInterface` it either needs to be signed with `com.apple.developer.usb.host-controller-interface` entitlement (which I don't have) or run as root on a system that has SIP completely disabled.

## Todo

- [ ] Level separated logging
- [ ] Isochronous Transactions
