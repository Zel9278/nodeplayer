# Windows installers

Windows packaging is managed by Velopack. The previous standalone WiX definition has
been replaced by [the shared packaging script](../velopack/README.md), which produces an
MSI, Setup.exe, a portable ZIP and the update packages/feed together.

The package is installed per-user, with AppUserModelID `Zel9278.nodeplayer` for both its
shortcut and process. Future updates are applied from About using Velopack's helper;
the app closes normally and saves its session before files are replaced. Bare developer
executables retain their fallback identity registration.
