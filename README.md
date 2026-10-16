# usbip-macos

A [USB/IP](https://usbip.sourceforge.net/) client for macOS.

## Disclaimer

This is highly experimental. Use at your own risk.

## Permissions

Because this tool uses `IOUSBHostControllerInterface` it either needs to be signed with the
`com.apple.developer.usb.host-controller-interface` entitlement (which I don't have) or run as
root with SIP disabled and the `amfi_get_out_of_my_way=1` boot-arg set.

## Usage

```sh
sudo usbip-macos -r <server> list
sudo usbip-macos -r <server> attach --busid 1-1.4
```

The device stays attached until you stop the client.

## Limitations

- Isochronous devices (most webcams, microphones and speakers) don't work. This is a macOS
  limitation: `IOUSBHostControllerInterface` doesn't properly support isochronous transfers.
- UAS disks work, but only as plain mass storage. This is also a macOS limitation:
  `IOUSBHostControllerInterface` doesn't support the bulk streams UAS needs.
- Hubs can't be attached; attach the devices behind them instead.
