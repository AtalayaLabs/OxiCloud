# Frontend components

Every reusable piece of the SvelteKit app, in one list.

**Why this file exists:** the frontend's recurring failure is not a missing
component, it is a *second* one. Three right-click menus, two boot splashes,
three hand-rolled swipe-to-delete rows — each written because nobody knew the
first existed. So: **read this before building any UI, and add to it when you
build something reusable.** A component that is not listed here will be
rewritten by the next person.

Two rules follow from that:

- **Reuse beats writing.** If something here is 80% right, extend it with a
  prop. One component with a flag is better than two that drift.
- **Generalise on the second caller, not the third.** When a second surface
  needs the same behaviour, move it into `lib/components/` *then* — not after
  the third copy has grown its own bugs.

Paths are relative to `frontend/src/lib/`. Components with inline
`#[cfg(test)]`-equivalent coverage have a sibling `*.test.ts`.

## Layout & chrome

| Component | What it is |
| --- | --- |
| `components/AppShell.svelte` | The app frame: sidebar, top bar, bell, user controls, mobile footer. Everything persistent across routes mounts here — including the player, so audio survives navigation. |
| `components/MobileNav.svelte` | The bottom footer at phone widths: Files · Media · Player · Notifications · Account. Entries are links to routes, never overlays (an overlay has no history entry, so the Android back gesture leaves it open). |
| `components/BrandMark.svelte` | Cloud glyph + product name. Shared by the sidebar header and the public-share page. |
| `components/AppSplash.svelte` | The one "nothing to show yet" screen, full-viewport and centred. Its twin in `app.html` covers the pre-hydration wait. |
| `components/LoaderMark.svelte` | The brand mark with two dashes orbiting it, for a wait long enough to deserve the logo rather than a bare spinner. |
| `components/SkeletonList.svelte` | Row placeholders while a list loads. |
| `components/EmptyState.svelte` | Icon + title + message for an empty or failed surface. Error icons use `--color-error-text`; `--color-danger-text` is white in both themes and vanished on light. |
| `components/Banner.svelte` | The only banner component. Read-only notices, ops advisories, degraded-mode warnings. |
| `components/Toaster.svelte` | Transient toasts from `stores/ui`. |

## Lists, rows & gestures

| Component | What it is |
| --- | --- |
| `components/GestureList.svelte` | **The row-gesture component.** A list whose rows reorder by grip (pointer events, so it works on touch), act on a left or right swipe, offer those same actions as buttons on hover, and report a tap and a long press. Capabilities are declared per list: `reorderCapable`, and one `SwipeAction` per direction — passing the action *is* the capability, so a flag and a handler cannot disagree. Each action reaches the user by the route their pointer has: swipe on touch, button on hover, and the button is also what gives it to the keyboard. Tones are `danger` (red) and `add` (blue) — not green, which red/green colour blindness collapses — and every backing carries its icon **and** its label, so colour is never the only channel. Used by the player queue, a playlist's tracks and the notification feed. Requires every row rendered, so not for a virtualised list. |
| `components/ResourceList.svelte` | The files/photos/shares table and grid: selection, sorting, grouping, context actions, drag-and-drop, virtualisation. The biggest component in the app; extend it rather than starting a list from scratch. |
| `components/VirtualList.svelte` | Windowing for uniform row heights. |
| `components/VirtualRows.svelte` | Windowing for rows that declare their own height (the photo timeline mixes headers and content). |
| `components/ListToolbar.svelte` | Sort / group / filter controls above a list. |
| `components/DisplayModeControls.svelte` | List-vs-grid and density toggles. |
| `components/ActionBar.svelte` | Two-slot bar above a `ResourceList`: page actions on the left, batch actions on the right. |
| `components/ActionMenu.svelte` | A "⋮" trigger and its menu, for table rows whose action column outgrew icons. |
| `components/ContextMenu.svelte` | **The** right-click menu: scrim, viewport clamping, keyboard navigation, danger entries via `ConfirmButton`, `href` entries as downloads. Everything is resolved by the caller — no per-item predicates. |

## Dialogs & inputs

| Component | What it is |
| --- | --- |
| `components/Modal.svelte` | The dialog shell: backdrop, focus handling, title, footer slot. |
| `components/DialogHost.svelte` | Renders `stores/dialogs` — app-wide confirm and prompt dialogs, so no page hand-rolls one. |
| `components/Button.svelte` | `primary` / `secondary` / `danger`, with icon and `small` variants. |
| `components/ConfirmButton.svelte` | Arms on the first click, runs on the second, swapping its label for "Confirm" in between. The destructive-action pattern everywhere. |
| `components/AboutDialog.svelte` | Version, build and licence information. |
| `components/MoveDialog.svelte` | Destination picker for move and copy, filtered to drives the caller can create in. |
| `components/DrivePicker.svelte` | Drive selector, for forms that target one. |
| `components/ShareDialog.svelte` | Grants, link shares, expiry and permissions for one resource. |
| `components/DedupSiblingsDialog.svelte` | The delete-UX chooser for a photo whose blob has other visible references. |
| `components/LanguageSelect.svelte` | Locale picker over the 16 bundled locales. |
| `components/QuotaEditor.svelte` | The storage-cap modal, shared by both admin surfaces that change one. |
| `components/CommandPalette.svelte` | ⌘K command palette. |

## Identity

| Component | What it is |
| --- | --- |
| `components/UserVignette.svelte` | Avatar + name + email + internal/external badge; resolves the profile lazily and cached. For share and recipient lists. |
| `components/UserAvatar.svelte` | The circle alone, for surfaces with no room for a name. |
| `components/SelfAvatar.svelte` | The signed-in user's own avatar. |
| `components/GroupVignette.svelte` | The same chip for a group subject, with its member count. |
| `components/OwnerAvatarStack.svelte` | Overlapping avatars for a drive's members. |
| `components/UserMenuContent.svelte` | The account menu's contents, shared by the top-bar dropdown and the mobile account page. |

## Media & editors

| Component | What it is |
| --- | --- |
| `components/MusicPlayer.svelte` | The app's player: the `<audio>` element (above the router, so playback survives navigation), the top-bar trigger and the popover holding `PlayerPanel`. |
| `components/PlayerPanel.svelte` | The player's controls — now playing, transport, progress, volume, queue. Placement is the caller's business: the popover and `/player` render the same component. |
| `components/PhotoLightbox.svelte` | Full-screen photo and video viewer, shared by the timeline, People and Places. |
| `components/PeopleView.svelte` | Face clusters from `GET /api/people`, drilling into the lightbox. |
| `components/PlacesMap.svelte` | Geotagged photos on a self-hosted MapLibre map, with server-side clusters. |
| `components/FileViewer.svelte` | Inline preview for a file by type. |
| `components/CollabEditor.svelte` | CodeMirror 6 + Yjs collaborative markdown editing. |
| `components/WopiEditor.svelte` | The WOPI iframe host for Office documents. |

## Notifications

| Component | What it is |
| --- | --- |
| `components/NotificationRow.svelte` | One persistent (bell) notification, rendered the same in the dropdown and on `/notifications`. |
| `components/TransientNotificationRow.svelte` | The same shape for a toast that has not been persisted. |

## Admin

| Component | What it is |
| --- | --- |
| `components/AdminJobsPanel.svelte` | The Jobs tab: registered jobs, run history, findings per run. |
| `components/AdminDrivePoliciesPanel.svelte` | Default Personal and Shared drive policies. |
| `components/DrivePoliciesModal.svelte` | Policy editor for one drive. |
| `components/PolicyList.svelte` | The list-of-toggles both policy surfaces are built from. |
| `components/admin/JobParamsDialog.svelte` | Trigger dialog built entirely from `JobSummary.parameters`, so it carries no per-job knowledge. |

## Navigation helpers

| Component | What it is |
| --- | --- |
| `components/FolderBreadcrumb.svelte` | The ancestor trail with its root icon (drive / share / link), a right-click menu per crumb, and a `reloadToken` for callers that mutate the tree. |

## Not components, but check here before writing one

- **`composables/`** — reusable rune logic: `useSelection` (range and toggle
  selection), `useVirtualWindow`, `useOwnerCache`, `useAsyncAction`,
  `useArmedConfirm`, `useNotifications`, `useTopic` / `useFolderTopic` /
  `useJobTopic` (message-bus subscriptions), `useReconnect`, `lazyComponent`.
- **`stores/`** — global reactive state: `session`, `ui`, `theme`,
  `preferences`, `dialogs`, `drives`, `files`, `player`, `serverConfig`,
  `serverStatus`, `adminAdvisories`.
- **`utils/`** — DOM and data helpers, including `dragAutoScroll` (edge
  auto-scroll, shared by the files browser's drag-and-drop and
  `GestureList`'s pointer drag), `menuKeyboard`, `dragModifiers`, `errors`,
  `appPath`.
- **`icons/Icon.svelte`** — the only way to render an icon; names come from
  the generated Font Awesome registry.

## Adding a component

1. Put it in `lib/components/` (or `lib/components/admin/` for admin-only).
2. Open it with a comment saying what it is and **why it exists** — the
   duplication it prevents, the bug it was extracted after. The existing
   files do this; it is what makes the next person reuse instead of rewrite.
3. Scoped `<style>` with BEM class names, colours only via `var(--*)`.
4. Add a row to the table above in the same commit.
5. Pin the behaviour that would regress silently in a sibling `*.test.ts`.
   For anything gesture- or geometry-driven, give the test real rects —
   jsdom's are all zero.
