#!/usr/bin/env bash
# The UI library policy, enforced mechanically.
#
# The console is locked to shadcn/ui (base-nova) over @base-ui/react. That lock
# had decayed into a hand-written component layer and a 591-line parallel
# stylesheet before anyone noticed, because nothing checked it. Review habit is
# not a gate; this is.
#
# Run standalone, or through `build.sh --check`.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "${HERE}"

fail=0
checks=0

note() { printf 'verify-ui-policy.sh: %s\n' "$1"; }
bad() {
    printf 'verify-ui-policy.sh: FAIL %s\n' "$1" >&2
    fail=$((fail + 1))
}

# Every file in the primitive directory must be named after a shadcn registry
# item. Adding one means adding its name here, which is the moment an author has
# to confirm the registry really ships it rather than hand-writing a lookalike.
REGISTRY_PRIMITIVES="
alert
alert-dialog
badge
button
card
combobox
dialog
dropdown-menu
empty
field
input
input-group
item
label
progress
scroll-area
select
separator
sheet
skeleton
spinner
switch
table
tabs
textarea
toast
toggle
toggle-group
tooltip
"

# --- 1. no forbidden component ecosystem ------------------------------------
checks=$((checks + 1))
forbidden='@radix-ui|@mui/|@mantine/|@chakra-ui/|"antd"|@headlessui/|@ariakit/|@nextui-org/|@park-ui/|react-aria-components|daisyui|flowbite'
if grep -nE "${forbidden}" bun.lock >/dev/null 2>&1; then
    bad "a forbidden UI ecosystem is in bun.lock:"
    grep -nE "${forbidden}" bun.lock | sed 's/^/    /' >&2
fi
if grep -rnE "from ['\"](@radix-ui|@mui|@mantine|@chakra-ui|antd|@headlessui|@ariakit)" src >/dev/null 2>&1; then
    bad "a forbidden UI ecosystem is imported under src/:"
    grep -rnE "from ['\"](@radix-ui|@mui|@mantine|@chakra-ui|antd|@headlessui|@ariakit)" src | sed 's/^/    /' >&2
fi

# --- 2. the shadcn style is still base-nova ---------------------------------
checks=$((checks + 1))
grep -q '"style": "base-nova"' components.json ||
    bad 'components.json no longer declares the base-nova style'
grep -q '"ui": "@/shared/components/ui"' components.json ||
    bad 'components.json no longer aliases ui to @/shared/components/ui'

# --- 3. the primitive directory holds registry items only -------------------
checks=$((checks + 1))
for path in src/shared/components/ui/*.tsx; do
    name="$(basename "${path}" .tsx)"
    printf '%s\n' "${REGISTRY_PRIMITIVES}" | grep -cx "${name}" >/dev/null ||
        bad "${path} is not a shadcn registry item; primitives are added with 'bun x shadcn add', never hand-written"
done

# --- 4. primitives are not re-styled from the stylesheet --------------------
# Reaching into a vendored primitive through its data-slot, or matching the
# utility classes it emits, is the parallel styling system the policy forbids:
# a variant rename upstream silently drops the rule.
checks=$((checks + 1))
slot_rules="$({ grep -cE '^\[data-slot=|^\.group..button' src/styles.css || true; } | tail -1)"
[ "${slot_rules}" = 0 ] ||
    bad "src/styles.css has ${slot_rules} rule(s) reaching into a vendored primitive; style the primitive instead"
# The prefers-reduced-motion block is exempt: cancelling an animation for
# accessibility is the one place !important is the correct tool, and it is a
# document default rather than a component style.
bang="$({ grep -n '!important' src/styles.css || true; } |
    { grep -vE 'animation-|transition-duration|scroll-behavior' || true; } | wc -l | tr -d ' ')"
[ "${bang}" = 0 ] ||
    bad "src/styles.css has ${bang} !important declaration(s) outside the reduced-motion block"

# --- 5. colour lives in tokens, in oklch ------------------------------------
checks=$((checks + 1))
if grep -rnE '#[0-9a-fA-F]{3,8}\b|rgba?\(|hsla?\(' src --include='*.tsx' >/dev/null 2>&1; then
    bad "a colour literal appears in a component; lift it into a token in src/styles.css:"
    grep -rnE '#[0-9a-fA-F]{3,8}\b|rgba?\(|hsla?\(' src --include='*.tsx' | sed 's/^/    /' >&2
fi
if grep -nE '#[0-9a-fA-F]{3,8}\b' src/styles.css >/dev/null 2>&1; then
    bad "src/styles.css carries a hex colour; the baseline is oklch:"
    grep -nE '#[0-9a-fA-F]{3,8}\b' src/styles.css | head -20 | sed 's/^/    /' >&2
fi

# --- 6. features compose components; they do not hand-roll controls ---------
checks=$((checks + 1))
raw_control_pattern='<table[ >]|<textarea[ >]|<select[ >]|<progress[ >]|role="dialog"|role="alertdialog"|role="radiogroup"|role="progressbar"|role="tablist"|className="callout|className={`callout|type="file"'
# Comment lines are excluded: a note explaining why a raw control was replaced
# names the thing it replaced, and a gate that fires on its own rationale
# teaches authors to delete the rationale.
raw_hits="$({ grep -rnE "${raw_control_pattern}" src/features src/app --include='*.tsx' 2>/dev/null || true; } |
    { grep -v '\.test\.' || true; } |
    { grep -vE '^[^:]+:[0-9]+: *(//|///|\*|/\*)' || true; })"
if [ -n "${raw_hits}" ]; then
    bad "a raw control or ad-hoc callout appears outside the component library:"
    printf '%s\n' "${raw_hits}" | sed 's/^/    /' >&2
fi

# --- 7. one component tree, one utils, one http module ----------------------
checks=$((checks + 1))
[ ! -d src/components ] ||
    bad 'src/components/ still exists; the component tree is src/shared/components/'
[ ! -e src/lib/utils.ts ] ||
    bad 'src/lib/utils.ts duplicates src/shared/lib/utils.ts'
[ ! -e src/lib/api.ts ] ||
    bad 'src/lib/api.ts re-exports src/shared/lib/http.ts; import the module itself'

# --- 8. the composite layer is the only thing features import ---------------
# A feature reaching for a primitive to build a control that a composite should
# own is how the eleven divergent confirm dialogs happened.
checks=$((checks + 1))
primitive_hits="$({ grep -rn "components/ui/alert-dialog" src/features --include='*.tsx' 2>/dev/null || true; } |
    { grep -v '\.test\.' || true; })"
if [ -n "${primitive_hits}" ]; then
    bad "a feature imports the alert-dialog primitive directly; use the ConfirmDialog composite:"
    printf '%s\n' "${primitive_hits}" | sed 's/^/    /' >&2
fi

# --- 9. route modules export their route and nothing else -------------------
# The router plugin only code-splits a route whose exports are the route. One
# re-export added for a test's convenience hoisted a whole feature page, and
# everything it imported, into the entry chunk — 48 kB that nothing on the
# first paint needs.
checks=$((checks + 1))
route_exports="$({ grep -rnE '^export (const|function|\{|default)' src/app/routes --include='*.tsx' 2>/dev/null || true; } |
    { grep -v 'export const Route' || true; } |
    { grep -v '/-' || true; })"
if [ -n "${route_exports}" ]; then
    bad "a route module exports something other than its Route, which defeats code splitting:"
    printf '%s\n' "${route_exports}" | sed 's/^/    /' >&2
fi

if [ "${fail}" -ne 0 ]; then
    note "${fail} violation(s) across ${checks} checks"
    exit 1
fi
note "${checks}/${checks} PASS"
