# Contributing to homeCore

Thanks for helping. The full guide is at
https://homecore.io/docs/development/contributing. In short:

- **Open an issue first** for a new feature, a new plugin or a change in
  behavior. A fix for a clear bug can go straight to a pull request.
- **Branch from `develop` and open the pull request against `develop`.**
  `main` only receives releases.
- **One change per pull request**, with a test for any change in behavior.
- **Run `just check`** before you push. It runs CI's formatting, Clippy and
  tests.
- **Leave out** git submodules, dependencies on repositories outside
  homeCore-io, generated or benchmark files, version bumps and release notes.

**Plugins** can live in this repository under `plugins/hc-<name>` (Rust), or
in a repository of your own: Python and Node.js plugins run in a plugin
runtime, and are listed in the Catalog through
[homeCore-io/registry](https://github.com/homeCore-io/registry/issues). A
service you run yourself can also add devices over MQTT. See "Contributing a
plugin" in the guide.

**SDK changes** have to work for every plugin, not one deployment: follow what
the existing calls send, keep the MQTT protocol as it is unless an issue agreed
otherwise, and add tests.

Contributions are licensed under MIT or Apache 2.0, at your option, like the
rest of homeCore.
