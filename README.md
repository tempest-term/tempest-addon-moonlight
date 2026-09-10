# Tempest Moonlight addon

## Installing from Tempest

Nothing to download by hand. Tempest fetches this addon the first time you use
a Moonlight/Sunshine connection, verifies it, and installs it. That is the
supported path on every platform.

## Installing from a file

For a machine that cannot reach GitHub, take the package to it on a USB stick
and install it there.

1. On a machine with a network, open the
   [releases page](https://github.com/tempest-term/tempest-addon-moonlight/releases)
   and download the `.tpx` for the target machine:

   | File | For |
   | --- | --- |
   | `moonlight-<version>-darwin-arm64.tpx` | macOS, Apple silicon |
   | `moonlight-<version>-win32-x64.tpx` | Windows, Intel/AMD |
   | `moonlight-<version>-win32-arm64.tpx` | Windows, Arm |
   | `moonlight-<version>-linux-x64.tpx` | Linux, Intel/AMD |
   | `moonlight-<version>-linux-arm64.tpx` | Linux, Arm |

2. Copy that one file across and point Tempest at it.

The signature travels inside the `.tpx`, so the single file is all you need and
Tempest checks it for you — there is nothing to verify by hand.

Pick the version Tempest asks for. It installs a specific build, not whatever
is newest, and refuses a package that is for another version, another machine
or another app.

A `.tpx` you build yourself is unsigned, and Tempest will not install it.
