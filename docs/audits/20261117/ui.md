# Home Mate UI / UX / Console Audit

Audit revision: Core `c3ed6c6` (the documentation-only successor of runtime baseline `05da212`); 2026-11-17. Read-only review; no credentials, devices, service state, or repository sources changed.

## Scope and confidence

Reviewed shared shell/router/auth, API request helper, resource/poll/save hooks, device store, status/device/settings/interaction/tool routes; deep reads of persona, household/working memory, fleet, prompts, maintenance, OpenAPI, follow-up, arbitration, volume, device metadata, and base input/dialog/logo/theme components. Followed persona management calls into Rust API/storage. Read the project boundaries and roadmap context. This is a broad code and contract audit, not a claim that every source file or browser path has been executed.

The root agent ran the frontend production build successfully. `web/package.json` has dev/build/preview scripts, no component/e2e test suite. Two isolated Vue-reactivity probes ran the actual extracted source expressions/watch callback without network; both reproduced the findings below. No live browser rendering or mobile/screen-reader interaction was performed. Therefore branding/layout assessments below concern source consistency and information architecture, not measured visual appeal or accessibility certification. No P0 was demonstrated in this audit scope.

## Verified findings

### UI-01 — P1: Persona editor never fetches initial Agent.md / USER.md and can overwrite the existing persona with an empty revision

Evidence:

- `web/src/components/settings/AgentPersonaCard.vue:26` and `:27` instantiate resources; neither is loaded on mount or poll. All `.load()` calls occur after mutations or on an error retry; the latter cannot become visible before any request.
- `web/src/hooks/useResource.ts:56` starts `data = null`; `:83` defines load; `:123` only registers disposal; the hook does not auto-fetch.
- `AgentPersonaCard.vue:29` initializes empty content; `:42–44` fall back to no revisions, version 0, no USER.md entries; `:160` disables Save only while busy.
- `AgentPersonaCard.vue:59–67` submits the empty content before the first read. `crates/hm-core/src/api/services/persona.rs:55` forwards it; `crates/hm-core/src/agent_md.rs:100–125` validates maximum size only and inserts even empty content as the latest revision.

Trigger: A previously configured system has nonempty Agent.md or USER.md records. Open Settings. The card shows v0, blank persona, empty profiles. Clicking Save writes an empty new Agent.md revision. Revision history is initially unavailable in the UI, so the user cannot inspect before replacing it. Existing historical revisions are retained within the normal bounded-history limit, so this is not necessarily irreversible erasure.

Fix: Load both resources on mount, distinguish loading/failed/empty/loaded, and disable editing/save until the initial authoritative read completes. Preserve dirty drafts on later refresh. Consider revision precondition/CAS to prevent another browser's persona edit being silently replaced.

Acceptance: Preseed a persona and a USER.md record; mounting the Settings card fetches and displays both plus revision history. Save cannot run before read success. Failed reads show retry without enabling an empty overwrite. Save and rollback read back the expected version.

### UI-02 — P2: Shared-memory connection form is overwritten every 15 seconds during editing

Evidence:

- `web/src/components/settings/WorkingMemoryCard.vue:43–46` polls shared status every 15 s.
- `:64–73` unconditionally copies returned `enabled`, `upload_enabled`, and `nas_base_url` into the editable refs, without dirty state.
- `:153–164` saves those same refs. The token draft is separate and survives, so a slow URL/token edit can leave a new token paired with the old URL.

Trigger: Change NAS URL or either toggle, pause for the next status poll, then Save. The edited values have silently reverted to the prior server values. Even an unchanged server response triggers the watcher because a new resource object is assigned.

Verification: Executed the actual watcher source with Vue refs; after changing URL from `old.example` to `new.example` and both toggles to true, supplying the next unchanged server response yielded `{enabled:false,upload:false,url:'http://old.example'}`.

Fix: Separate server snapshot and draft; initialize once, maintain dirty fields, and apply explicit reload/discard. Status counters may continue polling independently.

Acceptance: Edit all three fields, simulate multiple status responses, and confirm draft preservation; explicit reload discards; successful save clears dirty state and displays readback.

### UI-03 — P2: Fleet node identity input fails to initialize and looks modified on first load

Evidence:

- `web/src/components/settings/FleetCard.vue:51–54` begins `selfId=''` and derives dirty from `selfId !== fleet.self_id` once fleet exists.
- `:84–85` first assigns the fetched fleet, then checks `!selfIdDirty` before copying server ID to input.
- `:261` binds that blank input; `:270` enables Save whenever dirty.

Trigger: Open advanced Settings when `/fleet` returns any nonempty self_id. Assigning fleet makes dirty true immediately, preventing initialization. Re-reading or pinging peers repeats this; users see a blank identity field and an enabled save despite never editing it. Saving empty can reset an explicit identity depending on backend fallback policy; this audit does not claim a specific fallback outcome.

Verification: Executed the actual computed expression and assignment condition with Vue refs. Result: `{serverId:'audit-node',renderedInput:'',dirty:true}`.

Fix: Capture pre-fetch dirty state or maintain an explicit touched flag; initialize the first successful snapshot unconditionally, preserve only actual user edits on later refresh.

Acceptance: Initial nonempty identity displays immediately and Save is disabled. Editing while a refresh completes preserves the edit. Clean form follows external identity changes.

### UI-04 — P2: Failed device refresh advances the timestamp presented as last successful data

Evidence:

- `web/src/stores/devices.ts:119–146` preserves previous snapshots for failed requests (correct), but `:147` updates `updatedAt=Date.now()` unconditionally, including when all requests fail.
- `web/src/views/StatusPage.vue:32` names it `lastSuccessAt` and `:64–69` passes it to the stale-data notice.
- `web/src/components/ErrorNotice.vue:21–22,41–45` treats that value as the time the visible data was read.
- `web/src/views/DevicePage.vue:59` renders its request error only when there is no previous mode snapshot; once data exists the device page does not mark that older snapshot stale. The shell's separate `/status` request can still succeed while `/mode` or `/devices` fails, so its core-connection pill does not cover this condition.

Trigger: Successfully load devices, then repeatedly fail `/mode`/`/devices`. The UI keeps old hardware facts but gives a continuously advancing last-read time; on Device page even the resource-specific failure is hidden after first success. This undermines the console's diagnostic truthfulness without changing hardware.

Fix: Keep separate last-attempt and per-resource last-success times; derive freshness from the specific data displayed. Show stale notices on Device page as on Status page.

Acceptance: One successful snapshot followed by failed refreshes retains its original success timestamp; partial success updates only the successful resource's timestamp; stale device values are labeled.

## Additional low-priority observations (not counted as higher-severity bugs)

- `AgentPersonaCard.vue:141` persona textarea has neither an associated `<label>` nor `aria-label`/`aria-labelledby`. Nearby headings are not a programmatic field label. Add label association and validate with a screen reader/automated accessibility pass. Other generic inputs correctly use Vue `useId` and real labels.
- `web/src/api/client.ts:82–98` has an early fetch-rejection path outside the cleanup `finally` at `:131–134`; failed fetches leave the timeout pending until expiry and may retain an outer abort listener. This is a bounded timer/resource hygiene issue, not demonstrated user-visible outage.
- The same request helper prefers `error` over `detail` for non-2xx responses (`:109–115`), so APIs returning a code plus an explanatory detail may show only the code. Verify against each contract before standardizing an envelope.
- No component or browser regression test script exists in `web/package.json`. Build/type checking does not exercise mount effects, dirty-state races, last-success timestamps, or form/API sequencing—the verified defects above are examples of that gap.

## Strengths supported by code

- A real management console exists, with typed API calls, lazy routes, shared device selection, auth pairing, secret replacement inputs, and applied-state/readback work in several forms. This is materially beyond a mock UI.
- `useResource` handles cancellation/sequence and retry; `usePoll` avoids overlapping ticks and suppresses hidden-tab polling; `useSave` guards duplicate mutation submissions and centralizes feedback.
- Device selection uses exact gateway-ID matching for device actions; device-specific components are keyed to the selected ID. Volume distinguishes draft from reported level and waits for readback rather than showing a slider edit as an applied device fact.
- ConfirmDialog traps keyboard focus, supports Escape, restores focus, and initially focuses Cancel. Destructive household-memory deletions and maintenance actions have confirmations. Secret fields display set/unset instead of echoing stored secrets.
- OpenAPI configuration exposes discovered operations, explicit whitelist, effect classification and verification settings; saving retains the full discovered catalog. Fleet push exposes planning and per-target outcomes.
- Chinese keys define the English dictionary type, giving build-time key completeness. English dictionary scan found only intentional language selector names in Chinese; audio phrases remain configurable user content, not untranslated interface text.
- Design tokens centralize theme colors/spacing/type; root has light/dark modes, prepaint theme bootstrap, focus-visible styling, and reduced-motion support. Layout source includes responsive wrapping/media queries; this is not proof of all mobile viewports passing.

## Logo and product UX assessment (non-bug opportunities)

1. Brand mark has a coherent source: the Vue SVG supports icon/full variants and semantic brand colors; dark theme changes both mark and wordmark. `BrandLogo.vue` supplies an accessible brand name, and standalone public SVG is available. No source evidence justifies a wholesale logo redesign. Inspect real 16/32/48 px assets, dark/light screenshots, and small speaker packaging before judging legibility or distinctiveness.
2. Settings still exposes a long stack of household memory, raw Agent.md/USER.md editing, prompts, backup, maintenance and (advanced) fleet/working-memory controls. A future household-facing home screen could prioritize room, everyday actions, pending memory review, and explainable recent outcomes. Keep raw prompts/protocols in administration. This is a product improvement, separate from the verified mount bug.
3. First-class household roles, a safe member workflow, and consent/identity profile review would improve usability beyond a single technical admin persona. Do not confuse the existing advanced-mode toggle with an authorization boundary or pretend the deferred voice identity spike is already complete.
4. A single action timeline could link spoken request → selected room/device → tool/evidence → audible outcome, using existing trace/provenance data. This would reduce dependence on multiple diagnostics cards without changing the product's ownership model.
5. A per-device acoustic calibration workflow should clearly distinguish measured, configured, actually applied, and last verified values, with date/provenance. The audio team separately owns the confirmed profile-runtime defect; the workflow idea is broader and should not be counted as fixing that bug.
6. Persona changes would benefit from preview and diff, plus one-click return to known-good content. Revision history already exists, making this incremental instead of a replacement subsystem.

## Suggested next slice

Repair UI-01 through UI-04 together as a focused console truthfulness/draft-state batch, add behavior-level mount/poll/save regression coverage, independent acceptance review, and only then consider packaging an update. These source defects warrant fixes; this audit alone neither verifies current five-device live health nor authorizes a device rollout. Preserve serial69 until the parent review combines runtime findings and rollout evidence.


本次统一基线、优先级与验证边界见 [审计总报告](README.md)；隔离复现源码/输出见 [证据附录](evidence.md)。
