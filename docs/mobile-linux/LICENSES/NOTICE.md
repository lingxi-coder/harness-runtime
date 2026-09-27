# Android MobileLinux licensing notice

LingXi Android distributions that include MobileLinux are distributed as a
combined work under **GNU GPL version 3**. Components that were originally
available under MIT or Apache-2.0 retain their original copyright and license
notices.

The source and license baseline is fixed by
`docs/mobile-linux/mobile-linux-pins.json`:

| Component | Pin | License |
| --- | --- | --- |
| OpenMinis Android shell and PTY bridge | `9cf3a855fecd27bb5735b84cacbd56852a3ab8dd` | GPL-3.0-only |
| OpenMinis PRoot fork | `8cf13e997cdc9472997aae19df8050c073c9a86c` | GPL-2.0-or-later |
| talloc | 2.4.2 | LGPL-3.0-or-later |
| Base Alpine minirootfs | 3.21.3 | aggregate package licenses |
| Local-app Alpine rootfs | 3.24.2 | aggregate package licenses |
| Official Node.js source (musl build) | 26.9.0 | MIT |
| npm / npx (official npm tarball) | 12.0.2 | Artistic-2.0 |
| pnpm / native platform executable | 12.5.1 | MIT |
| TypeScript native compiler/LSP (Linux arm64/x64) | 7.0.2 | Apache-2.0 |
| Alpine Git | 2.54.0-r0 | GPL-2.0-only |
| Vite | 8.3.0 | MIT |
| Rolldown | 1.2.9 | MIT |
| Tailwind CSS / Oxide | 4.3.3 | MIT |
| Lightning CSS | 1.33.0 | MPL-2.0 |
| React / ReactDOM | 19.3.0 | MIT |
| Rolldown Linux musl bindings | 1.2.9 | MIT |
| Tailwind Oxide Linux musl bindings | 4.3.3 | MIT |

The complete GPL-3.0 text is stored at `LICENSE` in the pinned OpenMinis source.
The complete PRoot GPL-2.0 text is stored at `COPYING` in the pinned PRoot
source; its source headers grant GPL version 2 or any later version, and the
Android combined work selects GPLv3. Release packaging must include those exact files, this notice, the
rootfs package-license inventory, the source pin manifest, and a written offer
or durable URL for the corresponding source.

No reference-tree executable, loader, shared object, or rootfs archive is a
release input. Release binaries must be rebuilt from the pinned sources.

This notice is not legal advice.
