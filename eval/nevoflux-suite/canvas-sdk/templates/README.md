# Canvas SDK third-party micro-app templates

Phase 3d snapshots used by the `canvas-third-party-002/003/004` tasks. Each
subdirectory holds a single-file `index.html` micro-app with provenance
metadata.

| Template | Source pattern | License |
|---|---|---|
| `codepen-calculator` | Pattern based on CodePen "Vanilla JS Calculator" demos | MIT-style attribution; hand-rewritten for Phase 3d |
| `bootstrap-dashboard` | Bootstrap 5 official dashboard example shape | MIT (Bootstrap) |
| `vanilla-todo` | TodoMVC vanilla-JS example shape | MIT (TodoMVC) |

These are **hand-written** micro-apps shaped like the named third-party
demos. They are **not** verbatim copies — verbatim third-party copies are
deferred until we can confirm per-snapshot licensing. The intent is to
exercise the Canvas SDK against micro-app patterns that did not originate
inside NevoFlux, validating "准则 (d)" universality of the SDK design
specification.

Each template exposes a small `window.*` API the SDK can call into:

| Template | Window API |
|---|---|
| `codepen-calculator` | `window.add(n)`, `window.subtract(n)`, `window.clear_acc()`, `window.value()` |
| `bootstrap-dashboard` | `window.add_row(label, count)`, `window.refresh_table()`, `window.row_count()` |
| `vanilla-todo` | `window.add_todo(text)`, `window.toggle(idx)`, `window.list_todos()` |

To replace any template with a verbatim third-party snapshot:

1. Fetch the original.
2. Confirm license compatibility (MIT / Apache-2.0 / BSD preferred).
3. Update `source-url.txt` + `snapshot-date.txt` to point at the original.
4. Update the table above.
