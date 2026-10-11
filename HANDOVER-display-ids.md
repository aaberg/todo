# Handover: keep display IDs consistent between listing and commands

## Goal and scope

Fix finding 3 from the repository review: an ID printed by the default `todo list` can resolve to a different todo in `edit`, `done`, `undone`, `remove`, or `log`.

The user approved the design described below and requested this handover. This file records the plan only; no implementation changes have been made for this finding.

Keep the implementation scoped to display-ID consistency. Do not fix completion serialization, date-group sorting, authentication, account isolation, sync behavior, or other review findings as part of this change.

## Confirmed problem

The default listing and ID-based commands currently use different populations when assigning IDs:

- `List` builds display entries, filters out todos completed before today, then calls `sort_for_display`.
- `List --all` does not filter before sorting.
- `Edit`, `Done`, `Undone`, `Remove`, and `Log` sort the complete set of non-deleted todos before resolving an ID.
- `sort_for_display` both sorts entries and reassigns IDs starting at 1. Consequently, the preliminary assignment in `with_display_ids` does not prevent the mismatch.

Example with no intervening state changes:

| Todo | Status | ID in default list | ID used by commands and `list --all` |
| --- | --- | --- | --- |
| Old task, due today | Completed yesterday; hidden from default list | Not shown | 1 |
| Buy milk, due tomorrow | Open | 1 | 2 |

The user sees `1 Buy milk` and runs `todo remove 1`. The command instead deletes Old task. The same mismatch affects the other ID-based commands.

The repository review reproduced this behavior using the actual CLI with a disposable HOME and SQLite database: the default list showed an open task as ID 1, while `remove 1` deleted the hidden completed task and left the displayed task intact. That is historical verification; the review fixtures were removed. The handover author has checked the current source, not rerun that scenario.

This is not the expected ephemeral-ID behavior after adding, deleting, completing, editing, or syncing todos. It occurs even when the underlying state is unchanged.

## Current code map

Line references describe the source checked when this handover was written; navigate by symbol if they shift.

| File / symbol | Relevant behavior |
| --- | --- |
| `cli/src/main.rs:50–58`, `Commands::List` branch | Filters before sorting. This is the required implementation change. |
| `cli/src/main.rs:76–144`, ID-based command branches | Sort the complete todo set before resolving IDs. Preserve this behavior. |
| `cli/src/model.rs:94–99`, `with_display_ids` | Assigns preliminary 1-based IDs. No signature or behavior change is needed. |
| `cli/src/model.rs:104–108`, `resolve_display_id` | Resolves an existing display ID to the stable todo UUID. No change is needed. |
| `cli/src/model.rs:146–158`, `sort_for_display` | Sorts the full list and renumbers it. Preserve its current ordering and renumbering for this fix. |
| `cli/src/display.rs`, `print_list` | Prints the supplied IDs. Do not add presentation-time renumbering. |
| `README.md`, CLI usage documentation | After implementation verification, document that filtered lists can contain ID gaps and IDs remain ephemeral. |

## Chosen design

Use the complete sorted non-deleted todo set as the canonical ID mapping for every command. Visibility filtering must not change that mapping.

Change only the ordering of operations in the listing branch:

1. Build display entries from the complete folded todo set.
2. Call `sort_for_display` to sort and assign canonical IDs.
3. For the default list only, retain open todos and todos completed today.
4. Print the remaining entries without another sort or renumbering pass.

Proposed listing logic:

```rust
let mut display = with_display_ids(todos);
sort_for_display(&mut display, today);

if !all {
    display.retain(|(_, t)| {
        t.completed_on.is_none() || t.completed_on == Some(today)
    });
}

print_list(&display, today);
```

The ID-based commands already use the complete-list mapping. Do not change their visibility rules, add persistent numeric IDs, introduce another mapping helper, or change exported signatures merely to implement this reorder.

### Tradeoff: visible ID gaps

A filtered list can show IDs such as 2, 4, and 5. This is intentional: omitted IDs belong to hidden todos, and every shown ID keeps the same meaning as in `list --all` and the ID-based commands.

Do not filter all mutation commands to obtain contiguous IDs. That would make hidden completed todos inaccessible to commands such as `undone` and `log`.

### Remaining limitation

IDs remain ephemeral. A state change or a change in date-dependent ordering can still renumber them between invocations. UUIDs remain the stable identity. This fix establishes consistency for the same todo set and reference date; it does not promise persistent IDs.

## Implementation and verification plan

1. Recheck the listing and ID-resolution branches before editing, accounting for any intervening repository changes.
2. Move the existing `sort_for_display` call before the visibility filter in the `Commands::List` branch. Keep the filter predicate and sorting rules unchanged.
3. Add a deterministic behavioral regression for the wrong-target scenario, using isolated data and fixed UUIDs. Follow existing test conventions; test externally observable mapping and mutation targets, not source text or merely the order of helper calls.
4. Run the relevant regression and exercise the actual CLI against a disposable HOME/SQLite fixture. Do not touch the user's real `~/.todo` data.
5. After runtime proof, update the existing README usage documentation to explain filtered-list ID gaps and the remaining ephemeral-ID limitation. Remove any temporary fixtures or scripts.

### Regression and smoke scenario

Create a hidden todo completed before today whose sort position precedes an open visible todo. The original example uses a hidden todo due today and an open todo due tomorrow. Seed completion dates directly in the isolated event fixture rather than relying on a previously completed task in real user data.

For deterministic automated checks, control the reference date where the chosen test surface allows it. For actual CLI smoke checks, derive fixture dates from the local date and choose dates that preserve the intended ordering across a midnight boundary, for example hidden due tomorrow/completed a week ago and visible open due in a week. Do not introduce a production clock override solely for this fix.

Verify all of the following:

- The default list hides the old completed todo and shows the open todo with its canonical ID, including a gap when appropriate.
- The open todo has the same ID in `list` and `list --all` before any mutation.
- `todo log <shown-id>` identifies the displayed open todo, not the hidden completed one.
- `todo remove <shown-id>` appends a Delete event for the open todo's UUID, and the hidden completed todo remains present in the folded state.
- The hidden completed todo remains addressable by its canonical ID, for example with `log`.
- With no hidden todos, visible IDs remain contiguous from 1.
- An all-hidden fixture still prints `No todos.` in the default listing; `list --all` still shows those todos.
- Todos completed today remain visible under the unchanged filter predicate.

The separate `undone` serialization bug is out of scope. Do not make successful clearing of completion a prerequisite for verifying this ID-mapping fix; verify hidden-item resolution with `log` instead.

## Acceptance criteria

For an unchanged event set and reference date, every ID printed by either listing mode resolves to the same UUID in all ID-based commands. Default-list visibility remains unchanged, `list --all` continues to include completed todos, and filtering never renumbers survivors. No unrelated behavior is changed.
