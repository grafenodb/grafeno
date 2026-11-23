# Restricted capability: runFullTrust

Partner Center asks why a package declares a restricted capability
(Submission → Properties, "Why do you need the runFullTrust capability,
and how will it be used in your product?"). The text to give, in English
(the reviewers' language):

---

GrafenoDB is a Win32 desktop application written in Rust, packaged with
MSIX (desktop bridge). It needs runFullTrust because it is a regular
desktop executable: it runs a local database server (HTTP API and Model
Context Protocol server, listening on 127.0.0.1 by default) and console
tools, started from its Start menu entry or from its execution alias
`grafeno` in PowerShell and the Command Prompt.

It does not require elevation, does not install or start Windows
services, drivers or startup tasks, does not change system settings,
protected locations, the system PATH or firewall rules, and does not
download or run other code. It runs as the signed-in user, stores its
database and settings in the user's profile (`%USERPROFILE%\GrafenoDB`),
and starts only its own executable.

---

Before submitting, check the wording Partner Center shows for the field
(it may change) and that the description still matches the package: the
manifest declares runFullTrust and nothing else (validate.ps1 checks it).
