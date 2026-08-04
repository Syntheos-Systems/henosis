# Install Henosis desktop

Henosis desktop is the graphical client for an existing Rift room service. The
supported setup path does not require a terminal. It does require three things
from the person or team operating Rift:

- the Rift service address, such as `https://rift.example.com`;
- a Rift username; and
- a Rift password.

Henosis desktop does not start or install Rift, PostgreSQL, or a managed cloud
service. If you do not have a Rift address and account, contact the operator of
the Rift service you intend to use.

> **Current alpha availability:** `v0.1.0-alpha.6` contains headless archives
> only. It does not contain desktop installers. The filenames below are the
> enforced contract for the next desktop-enabled release, not a claim about the
> assets attached to `v0.1.0-alpha.6`.

## Choose one installer

Open [Henosis releases](https://github.com/Syntheos-Systems/henosis/releases)
and select the newest release that includes desktop assets. Download exactly
one file from this table.

| Computer | Installer | What to do |
| --- | --- | --- |
| Ubuntu, Debian, or a compatible x86-64 Linux system | `henosis-desktop-{version}-linux-x86_64.deb` | Open the downloaded package with the system software installer and choose **Install**. |
| Other x86-64 Linux desktop | `henosis-desktop-{version}-linux-x86_64.AppImage` | Open **Properties**, allow the file to run as a program, then open it. |
| Apple silicon Mac | `henosis-desktop-{version}-macos-aarch64.dmg` | Open the disk image and drag Henosis into **Applications**. |
| Intel Mac | `henosis-desktop-{version}-macos-x86_64.dmg` | Open the disk image and drag Henosis into **Applications**. |
| x86-64 Windows PC | `henosis-desktop-{version}-windows-x86_64.exe` | Open the installer and follow its visible steps. |

If you are unsure which Mac you have, open **Apple menu > About This Mac**. A
Mac showing an Apple chip uses `aarch64`; a Mac showing an Intel processor uses
`x86_64`.

## Current trust warnings

Alpha packages are built by the public release workflow and receive checksums
and GitHub provenance attestations, but they are not yet store-trusted:

- the macOS application is ad hoc signed and is not Apple-notarized;
- the Windows installer is not Authenticode-signed; and
- Henosis does not yet provide an in-app updater.

macOS or Windows can therefore display an identity or reputation warning. Only
continue if the file came from the official Henosis release page. On macOS,
Control-click Henosis in **Applications**, choose **Open**, then confirm **Open**.
On Windows, review the SmartScreen details before choosing the option to run the
installer. These actions approve only the downloaded application; do not
disable operating-system security globally.

Checksums and provenance provide stronger optional verification. The commands
for that advanced path are in [Verify a release](../README.md#verify-a-release),
but verification is not a hidden prerequisite for using the graphical setup.

## Connect on first run

1. Open Henosis.
2. Confirm that the setup rail shows **Install** complete and **Connect** as the
   current step.
3. Enter the Rift service address, username, and password supplied by your Rift
   operator.
4. Choose **Connect and open rooms**.
5. Henosis opens the room selector with the most recently active room first.

If you already run Rift on the same computer at its default listener, choose
**Use an already-running local Rift** to fill `http://127.0.0.1:3200`. That
shortcut only fills the address. It does not launch or provision Rift.

Henosis sends the password directly to its native process. The password and
Rift tokens are not stored in browser storage. The saved profile contains only
the normalized service address and username; the native room cache contains
sanitized room summaries.

## Recover without a terminal

| What Henosis shows | What to check |
| --- | --- |
| Rift cannot be reached | Confirm the service address, your network connection, and whether the Rift operator reports an outage. Then try again. |
| Username or password was rejected | Re-enter the password or ask the Rift operator to confirm the account. Henosis clears the rejected password field. |
| Rift returned an unexpected response | Confirm that the address points to a compatible Rift service root, without an `/api` path. |
| Saved connection data cannot be read or written | Check available disk space and application-data permissions, then reopen Henosis. The application does not silently delete saved data. |

A failed network, authentication, protocol, or initial room request does not
replace the last known-good profile or room cache. Each saved file is replaced
atomically, but the profile and cache are not presented as one cross-file
transaction. If only one replacement succeeds, Henosis rejects mismatched cache
identity data and returns to the connection screen.
