# kvasilloni

A drop-in replacement for Kvaser's **`canlib32.dll`** that bridges a Windows CAN
application to a **Linux `vcan`** over the network using
[cannelloni](https://github.com/mguentner/cannelloni) - no Kvaser hardware, no
kernel driver.

The intended use case: CAN software running in a **Windows 11 VM** (that drives
the Kvaser CANlib API) needs to talk to CAN software bound to a **virtual CAN bus
on a separate Linux host**.

```
 Windows 11 VM                                       Linux host
+---------------------------+                      +---------------------------+
| CAN application           |      cannelloni      | cannelloni -I vcan0 ...   |
|  canlib32.dll  (SHIM)     +------ UDP/TCP -------+ vcan0  <->  your CAN code |
+---------------------------+  (LAN / host link)   +---------------------------+
```

Instead of touching hardware, the shim **is itself a cannelloni peer**: it speaks
cannelloni's wire format directly to a stock `cannelloni` process on the Linux
side. It carries classic CAN and CAN FD frames in both directions. Nothing else
is needed on Linux beyond `cannelloni` and a `vcan`.

## 1. Get the DLL

Download the latest per-arch zip from the
[Releases page](https://github.com/aalarchiv/kvasilloni/releases):

- the **`x86`** zip - 32-bit Windows apps (most legacy Kvaser apps)
- the **`x64`** zip - 64-bit Windows apps

Each zip contains `canlib32.dll`, `kvasilloni.ini.example`, this README, and the
license texts. Optionally verify it with `sha256sum -c SHA256SUMS.txt`.

(To build from source instead, see [docs/DEVELOPMENT.md](docs/DEVELOPMENT.md).)

## 2. Start cannelloni on Linux

Bring up a virtual CAN bus and run cannelloni. Replace `<win-ip>` with the
Windows machine's address:

```bash
sudo modprobe vcan
sudo ip link add dev vcan0 type vcan
sudo ip link set up vcan0
cannelloni -I vcan0 -R <win-ip> -r 20000 -l 20000
```

Your own Linux CAN code talks to `vcan0` as usual.

## 3. Install the DLL on Windows

1. Copy **`canlib32.dll`** next to the application's **`.exe`** (Windows loads it
   from the app's own folder first; back up the genuine Kvaser DLL if one is
   there).
2. Copy **`kvasilloni.ini.example`** to **`kvasilloni.ini`** in the same folder
   and point it at the Linux host:

   ```ini
   [cannelloni]
   host      = <linux-ip>     ; the Linux host running cannelloni
   port      = 20000          ; must match cannelloni's -l
   localport = 20000          ; must match cannelloni's -r; unique per app
   proto     = udp            ; udp | tcp
   ```
3. Start the app and pick **Interface Type = Kvaser** (or however it opens a
   Kvaser channel). That's it - CAN frames now cross the link both ways.

`kvasilloni.ini.example` documents every other setting. Any setting can also be
given as a `KVASILLONI_*` environment variable, which wins over the INI.

## Using TCP instead of UDP

TCP gives reliable, ordered delivery (better when packet loss would corrupt
multi-frame NMEA 2000 transport-protocol messages). Run the shim as TCP client:

```bash
# Linux:   cannelloni -C s -R <win-ip> -I vcan0 -l 20000
# Windows: in kvasilloni.ini set  proto = tcp   (tcprole = client)
```

In TCP mode `canOpenChannel` blocks while it connects (up to `connecttimeout`),
so open the channel off any UI/watchdog thread if a fast open matters. UDP opens
don't block.

To run the shim as the TCP **server** instead, set `tcprole = server` (cannelloni
connects as the client, `-C c`). The server is a **one-to-one tunnel**: it accepts
a single cannelloni client - the first allowed connection - then stops listening.
It serves one client per channel, not many, and a dropped client cannot reconnect
without reopening the channel. A dropped link is observable: `canReadStatus` sets
`canSTAT_BUS_OFF`, so the app can detect it and reopen.

## Troubleshooting

- **Frames go out but nothing comes back.** By default the shim (like cannelloni's
  `-R`) only accepts traffic from `host`. Make sure `host` is the IP cannelloni
  actually sends *from* (matters behind NAT or on a multi-homed box). Widen it
  with `allow = ip1, ip2`, or set `peercheck = off` to accept any source.
- **Received CAN FD frames are truncated to 8 bytes.** Open the channel with the
  `canOPEN_CAN_FD` flag and give `canRead` a 64-byte buffer.
- **The open fails with the port in use.** In UDP mode each app needs a unique
  `localport`. Pick a free one (and match cannelloni's `-r`).
- **No log file appears.** Set `KVASILLONI_LOG` to an **absolute** path - a
  relative path resolves against the host process's working directory (often
  `C:\Windows\System32`), not your app folder.

## More

- **Settings reference** - every key, with comments: `kvasilloni.ini.example`
- **Supported functions / retargeting to another app** -
  [docs/RETARGETING.md](docs/RETARGETING.md)
- **Building, testing, internals** - [docs/DEVELOPMENT.md](docs/DEVELOPMENT.md)

> Upgrading from 0.2.0? Existing INI files keep working, but three defaults
> changed: `peercheck` is now on, CAN FD receive needs `canOPEN_CAN_FD`, and a
> busy UDP `localport` now fails the open (set `udpportfallback = on` for the old
> behavior). `tools/ini_merge.py old.ini -o kvasilloni.ini` (needs Python 3)
> migrates a config into the commented 0.3.0 template.

## License

Licensed under the **GNU Lesser General Public License v3.0 or later**
(LGPL-3.0-or-later); see [`COPYING.LESSER`](COPYING.LESSER) and
[`COPYING`](COPYING). The shim contains no Kvaser or cannelloni source - it is an
independent implementation of the CANlib export interface that speaks the
cannelloni wire protocol to a stock, separately-running `cannelloni`. LGPL keeps
the library itself open and user-replaceable while leaving the apps that load it
(including proprietary ones) unaffected.
