# Computer, browser and native device tools

Use the tools actually registered by this host. A platform name, connected
device ID or SDK interface alone is not an automation backend.

| Surface | What establishes an executable entry |
| --- | --- |
| Desktop Computer Use | Configured native backend, current tool schema and OS permission |
| Browser | Connected supported browser integration and authenticated host descriptor |
| Android UI automation | Direct-channel native implementation and granted capability |
| Mobile native actions | The configured notification, clipboard, location, share, calendar, contacts or URL host service |

The current native desktop Computer backend is macOS-specific. Android Direct
and Play builds differ; the Play feature provider can intentionally return no
Computer Use backend. Do not install or emulate an unavailable privileged path.

Browser automation uses the host's browser integration (extension/native
messaging and supported browser control). A page already open in a browser is
not proof that this session has connected automation. Use an exposed status or
connection UI before claiming control. Follow the selected browser/tab context.

Where present, `mobile-device` and `mobile-personal-context` provide the current
native workflow. Calendar and contacts are bounded reads in the current tool
surface; do not promise unsupported writes. A successful `open_url` proves
handoff, not that the destination completed the user's task. A successful share
sheet invocation is not proof that a recipient received anything.

Use the `media` topic for camera/microphone operations. Native permissions and
resource readiness remain enforced even when another tool can identify a device.
Report denied/unavailable/canceled outcomes without manufacturing success.

Source anchors: Harness `crates/tools/{computer-use,android-use,mobile}`,
`crates/platforms/macos-computer-control`; product browser integration and
Android channel feature providers.
