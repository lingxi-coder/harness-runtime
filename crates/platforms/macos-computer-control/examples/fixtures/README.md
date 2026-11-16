# Isolated app-scoped acceptance

The fixture owns only a temporary text field, Apply button, menu and JSON
receipt. The backend example accepts only its fixed bundle identifier. Neither
requests, resets nor grants TCC permission. No real applications or clipboard
contents are changed. Coordinate targeting uses the fixture's current AX tree.

Build the fixture into a **new** directory (substitute absolute local paths):

```sh
mkdir -p /absolute/new/Fixture.app/Contents/MacOS
swiftc examples/fixtures/AppScopedFixture.swift -o /absolute/new/Fixture.app/Contents/MacOS/AppScopedFixture
cp examples/fixtures/Info.plist /absolute/new/Fixture.app/Contents/Info.plist
codesign --force --sign - /absolute/new/Fixture.app
open -gn -a /absolute/new/Fixture.app --args /absolute/new/receipt.json
cargo run --offline --example app_scoped_acceptance -- com.lingxi.acceptance.app-scoped-fixture /absolute/new/receipt.json /absolute/new/acceptance
```

Run package-owned acceptance through its real host separately. A successful
standalone example establishes only its own process's access and fixture AX
behavior. It does not establish authorization attribution or behavior of an old
installed Desktop build. The example fails without prompting if its native
backend process does not already have both OS permissions.

The example observes JPEG/AX capture, actual replacement text, button press,
menu press, rejection of a stale window id, and before/after frontmost app and
real cursor. A person moving the cursor or switching apps during the run affects
the last comparisons. No GUI run is claimed by merely compiling these sources.
