# GrafenoDB privacy policy — DRAFT

**Status: draft for the owner's review. Not legal advice; not published.**
Fill in the contact, have it reviewed, publish it at a stable URL (for
example the Grafeno site or a page in this repository), and give that URL
in Partner Center (Properties → Privacy policy URL).

What it states was checked against the source on 2026-11-22: the engine has
no telemetry, analytics, update check or crash reporting; it makes network
requests only to destinations the user configures (event webhooks, cluster
members, a Grafeno server named with `--url`). Check it again when that
changes.

---

## GrafenoDB privacy policy

*Last updated: <date of publication>*

GrafenoDB ("the app") is a graph database engine that runs on your
computer. This policy explains what happens to information when you use
the app distributed through the Microsoft Store.

### What the app stores, and where

- **Your database**: the nodes, facts, texts, vectors and other data you or
  your programs write into it, in a file on your computer
  (`%USERPROFILE%\GrafenoDB\data\grafeno.redb` by default, or where you
  choose).
- **Settings and credentials**: the settings file
  (`%USERPROFILE%\GrafenoDB\grafeno.toml`) holds the server's access token,
  created at random on first start. Accounts, password hashes, API keys
  and the audit log you enable are stored in the database. Both are
  readable only by your account and the computer's administrators.
- **Backups** you make go where you put them.

We, the publisher, do not receive, see or keep any of it.

### Network communication

- The app runs a server for your programs and agents (HTTP and MCP). By
  default it listens only on this computer (127.0.0.1); it accepts
  connections from other computers only if you change its address.
- The app sends data over the network only to destinations you configure:
  webhooks for event subscriptions, other members of a cluster, or a
  Grafeno server you connect a command to. It contacts no service of the
  publisher.
- The app has **no telemetry, analytics, advertising, update checks or
  crash reporting**. Updates come through the Microsoft Store, under
  Microsoft's terms.

### Personal information

The app processes personal information only if you put it in your
database; it is then under your control, on your computer. You are
responsible for the data you store and for the access you give to others
through accounts and keys.

### Retention and deletion

Your data stays until you delete it. Uninstalling the app does not delete
your database or settings, so that they are not lost by accident: to
remove them, delete the `%USERPROFILE%\GrafenoDB` folder (and any backups
or databases you put elsewhere).

### Children

The app is a developer tool and is not directed at children.

### Changes

Changes to this policy are published at this address with a new date.

### Contact

<publisher name and contact email or address — to be filled in by the
owner>

Source code: https://github.com/grafenodb/grafeno
