#!/usr/bin/env python3
"""Security contract tests for repository GitHub Actions workflows."""

from __future__ import annotations

from pathlib import Path
import re
import unittest

ROOT = Path(__file__).resolve().parents[1]
WORKFLOW_DIR = ROOT / ".github" / "workflows"
PINNED_ACTION = re.compile(r"^[^@\s]+@[0-9a-f]{40}$")
CANONICAL_USES_LINE = re.compile(
    r"^(?P<indent> *)(?P<dash>- +)?uses: *(?P<target>[^#\s]+)(?: +#.*)?$",
    re.MULTILINE,
)
REMOTE_SMOKE_SECRETS = {
    "TETHER_SMOKE_REMOTE_DIRECTORY",
    "TETHER_SMOKE_REMOTE_KNOWN_HOSTS",
    "TETHER_SMOKE_REMOTE_TARGET",
}
UPSTREAM_DAILY_CRON = "23 5 * * *"
UPSTREAM_WEEKLY_MACOS_CRON = "41 5 * * 0"
# Publication is the one act that needs write access, so it is the one job that
# gets it: a single override, in a single named job, holding a single scope.
PUBLISH_WORKFLOW = "release.yml"
PUBLISH_JOB = "publish"
PUBLISH_PERMISSIONS = ["contents: write"]


class WorkflowContractError(ValueError):
    pass


def top_level_block(text: str, key: str) -> str:
    lines = text.splitlines()
    marker = f"{key}:"
    try:
        start = lines.index(marker) + 1
    except ValueError as error:
        raise WorkflowContractError(f"missing block-form {marker}") from error
    block: list[str] = []
    for line in lines[start:]:
        if line and not line[0].isspace():
            break
        block.append(line)
    return "\n".join(block)


def indented_block(text: str, key: str, indent: int) -> str:
    lines = text.splitlines()
    marker = " " * indent + f"{key}:"
    try:
        start = lines.index(marker) + 1
    except ValueError as error:
        raise WorkflowContractError(f"missing block-form {marker.strip()}") from error
    block: list[str] = []
    for line in lines[start:]:
        if line.strip() and not line.startswith(" " * (indent + 1)):
            break
        block.append(line)
    return "\n".join(block)


def block_entries(block: str) -> list[str]:
    return [line.strip() for line in block.splitlines() if line.strip()]


def workflow_job_names(text: str) -> list[str]:
    return [
        line.strip()[:-1]
        for line in top_level_block(text, "jobs").splitlines()
        if re.fullmatch(r"  [A-Za-z0-9_-]+:", line)
    ]


def validate_scoped_write_permissions(name: str, text: str) -> None:
    """Only the publishing job may hold a permission the workflow root does not."""
    overrides = [
        indent
        for indent in re.findall(r"^( *)permissions *:", text, re.MULTILINE)
        if indent
    ]
    if name != PUBLISH_WORKFLOW:
        if overrides:
            raise WorkflowContractError(
                f"{name} must not override permissions below the workflow level"
            )
        return
    if overrides != ["    "]:
        raise WorkflowContractError(
            f"{name} must override permissions exactly once, in one job"
        )
    job = workflow_job_block(text, PUBLISH_JOB)
    if block_entries(indented_block(job, "permissions", 4)) != PUBLISH_PERMISSIONS:
        raise WorkflowContractError(
            f"{name} {PUBLISH_JOB} permissions are not exactly {PUBLISH_PERMISSIONS}"
        )


def validate_workflow(name: str, text: str) -> None:
    if "\t" in text:
        raise WorkflowContractError(f"{name} contains a tab")
    if re.search(r"^ *-? *(?:\"[^\"\n]*\"|'[^'\n]*') *:", text, re.MULTILINE):
        raise WorkflowContractError(f"{name} contains a quoted mapping key")
    flow_mappings = re.findall(
        r"^(?: *- *)?\{.*$|^ *[A-Za-z0-9_-]+ *: *\{(?!\{).*$",
        text,
        re.MULTILINE,
    )
    allowed_empty_permissions = name == "upstream-canary.yml" and flow_mappings == [
        "permissions: {}"
    ]
    if flow_mappings and not allowed_empty_permissions:
        raise WorkflowContractError(f"{name} contains a flow-style mapping")
    on_block = top_level_block(text, "on")
    if allowed_empty_permissions:
        permissions: list[str] = []
    else:
        permissions = [
            line.strip()
            for line in top_level_block(text, "permissions").splitlines()
            if line.strip()
        ]
    expected_permissions = [] if name == "upstream-canary.yml" else ["contents: read"]
    if permissions != expected_permissions:
        raise WorkflowContractError(
            f"{name} permissions do not match the workflow trust boundary"
        )
    validate_scoped_write_permissions(name, text)
    if "pull_request_target:" in on_block:
        raise WorkflowContractError(f"{name} must not use pull_request_target")

    lines = text.splitlines()
    uses_matches = list(CANONICAL_USES_LINE.finditer(text))
    possible_uses = re.findall(
        r"^.*(?:^|[-{,\s])uses\s*:.*$", text, re.MULTILINE
    )
    if len(possible_uses) != len(uses_matches):
        raise WorkflowContractError(f"{name} contains a noncanonical uses entry")
    for match in uses_matches:
        target = match.group("target")
        if not target.startswith("./") and not PINNED_ACTION.fullmatch(target):
            raise WorkflowContractError(
                f"{name} external action is not pinned to a full commit SHA"
            )
        if not target.startswith("actions/checkout@"):
            continue
        uses_line = text.count("\n", 0, match.start())
        uses_indent = len(match.group("indent"))
        step_indent = uses_indent if match.group("dash") else max(uses_indent - 2, 0)
        end = len(lines)
        for index in range(uses_line + 1, len(lines)):
            line = lines[index]
            if line.startswith(" " * step_indent + "- "):
                end = index
                break
        step = "\n".join(lines[uses_line:end])
        if not re.search(r"^\s+persist-credentials:\s*false\s*$", step, re.MULTILINE):
            raise WorkflowContractError(
                f"{name} checkout must set persist-credentials: false"
            )

    if re.search(r"^\s+pull_request:\s*$", on_block, re.MULTILINE):
        if re.search(r"\bsecrets\b", text) or re.search(
            r"^\s+inherit\s*$", text, re.MULTILINE
        ):
            raise WorkflowContractError(
                f"{name} pull-request workflow must not reference or pass secrets"
            )


def validate_live_product_boundaries(workflows: dict[str, str]) -> None:
    untrusted = workflows["live-product.yml"]
    untrusted_on = top_level_block(untrusted, "on")
    if not re.search(r"^\s+pull_request:\s*$", untrusted_on, re.MULTILINE):
        raise WorkflowContractError("live-product.yml must be pull-request triggered")
    if re.search(r"^\s+(?:push|workflow_dispatch):\s*$", untrusted_on, re.MULTILINE):
        raise WorkflowContractError("live-product.yml must contain only untrusted triggers")
    if "uses: ./.github/workflows/live-product-core.yml" not in untrusted:
        raise WorkflowContractError("live-product.yml must call the shared smoke workflow")

    trusted = workflows["live-product-trusted.yml"]
    trusted_on = top_level_block(trusted, "on")
    if not all(trigger in trusted_on for trigger in ("  push:", "  workflow_dispatch:")):
        raise WorkflowContractError("trusted smoke must support push and manual triggers")
    if "pull_request:" in trusted_on or "pull_request_target:" in trusted_on:
        raise WorkflowContractError("trusted smoke must not run for pull requests")
    trusted_job = workflow_job_block(trusted, "live-product-main")
    main_conditions = re.findall(r"^    if:.*$", trusted_job, re.MULTILINE)
    if main_conditions != ["    if: github.ref == 'refs/heads/main'"]:
        raise WorkflowContractError("secret-bearing smoke must be restricted to main")
    if "uses: ./.github/workflows/live-product-core.yml" not in trusted_job:
        raise WorkflowContractError("trusted main smoke must call the shared workflow")
    passed_pairs = re.findall(
        r"^      ([A-Z][A-Z0-9_]+):\s*\$\{\{\s*secrets\.([A-Z][A-Z0-9_]+)\s*\}\}\s*$",
        trusted_job,
        re.MULTILINE,
    )
    referenced = set(
        re.findall(
            r"\$\{\{\s*secrets\.([A-Z][A-Z0-9_]+)\s*\}\}", trusted_job
        )
    )
    expected_pairs = {(name, name) for name in REMOTE_SMOKE_SECRETS}
    if set(passed_pairs) != expected_pairs or len(passed_pairs) != len(expected_pairs):
        raise WorkflowContractError("trusted smoke secret mapping is not exact")
    if referenced != REMOTE_SMOKE_SECRETS:
        raise WorkflowContractError("trusted smoke secret allowlist is not exact")
    if re.search(r"^\s+inherit\s*$", trusted, re.MULTILINE):
        raise WorkflowContractError("trusted smoke must pass named secrets, not inherit")

    non_main_job = workflow_job_block(trusted, "live-product-non-main")
    non_main_conditions = re.findall(r"^    if:.*$", non_main_job, re.MULTILINE)
    if non_main_conditions != ["    if: github.ref != 'refs/heads/main'"]:
        raise WorkflowContractError("non-main smoke must exclude main")
    if "uses: ./.github/workflows/live-product-core.yml" not in non_main_job:
        raise WorkflowContractError("non-main smoke must call the shared workflow")
    if re.search(r"\bsecrets(?:\.|\[|:)", non_main_job):
        raise WorkflowContractError("non-main smoke must not receive secrets")

    core = workflows["live-product-core.yml"]
    core_on = top_level_block(core, "on")
    if "  workflow_call:" not in core_on or re.search(
        r"^\s+(?:push|pull_request|pull_request_target|workflow_dispatch):\s*$",
        core_on,
        re.MULTILINE,
    ):
        raise WorkflowContractError("live-product core must be reusable-call only")
    if re.search(r"^\s+environment:\s*", core, re.MULTILINE):
        raise WorkflowContractError(
            "live-product core must not acquire environment secrets"
        )
    declared = set(
        re.findall(r"^      ([A-Z][A-Z0-9_]+):\s*$", core_on, re.MULTILINE)
    )
    referenced = set(
        re.findall(r"\$\{\{\s*secrets\.([A-Z][A-Z0-9_]+)\s*\}\}", core)
    )
    optional_count = len(
        re.findall(r"^        required:\s*false\s*$", core_on, re.MULTILINE)
    )
    if (
        declared != REMOTE_SMOKE_SECRETS
        or referenced != REMOTE_SMOKE_SECRETS
        or optional_count != len(REMOTE_SMOKE_SECRETS)
    ):
        raise WorkflowContractError("live-product core secret allowlist is not exact")


def validate_upstream_canary_boundaries(workflows: dict[str, str]) -> None:
    canary = workflows["upstream-canary.yml"]
    on_block = top_level_block(canary, "on")
    top_level_triggers = re.findall(r"^  ([A-Za-z0-9_-]+)\s*:\s*$", on_block, re.MULTILINE)
    if top_level_triggers != ["schedule", "workflow_dispatch"]:
        raise WorkflowContractError("upstream canary triggers must be exactly schedule and manual")
    for cron in (UPSTREAM_DAILY_CRON, UPSTREAM_WEEKLY_MACOS_CRON):
        if on_block.count(f'cron: "{cron}"') != 1:
            raise WorkflowContractError("upstream canary cadence is not exact")
    if re.search(r"\bsecrets\b", canary) or re.search(
        r"^\s+environment:\s*", canary, re.MULTILINE
    ):
        raise WorkflowContractError("upstream canary must not access secrets")
    if "permissions: {}" not in canary or "actions/checkout@" in canary:
        raise WorkflowContractError("upstream canary must not receive a repository token")
    if "continue-on-error:" in canary:
        raise WorkflowContractError("upstream canary failures must stay visible")
    if canary.count("git -c protocol.version=2 ls-remote --exit-code") != 1:
        raise WorkflowContractError("Herdr master must be resolved exactly once")
    exact_checkout_counts = {
        'fetch --quiet --no-tags --depth=1 tether "$GITHUB_SHA"': 2,
        'checkout --quiet --detach "$GITHUB_SHA"': 2,
        'test "$(git -C "$GITHUB_WORKSPACE" rev-parse HEAD)" = "$GITHUB_SHA"': 2,
        'upstream_dir="$RUNNER_TEMP/herdr-upstream"': 1,
        'git -C "$upstream_dir" fetch --quiet --no-tags --depth=1 origin "$HERDR_SHA"': 1,
        'git -C "$upstream_dir" checkout --quiet --detach "$HERDR_SHA"': 1,
        'if [[ "$actual_sha" != "$HERDR_SHA" ]]; then': 1,
        'git -C "$upstream_dir" symbolic-ref --quiet HEAD': 1,
    }
    if any(
        canary.count(fragment) != count
        for fragment, count in exact_checkout_counts.items()
    ):
        raise WorkflowContractError("upstream canary checkout is not exact and detached")
    for variable in (
        "ACTIONS_ID_TOKEN_REQUEST_TOKEN",
        "ACTIONS_RUNTIME_TOKEN",
    ):
        if canary.count(f'          {variable}: ""') != 3:
            raise WorkflowContractError("external canary execution must scrub runner service data")
    unset_commands = (
        "unset GITHUB_ENV GITHUB_OUTPUT GITHUB_PATH GITHUB_STATE GITHUB_STEP_SUMMARY"
    )
    if len(re.findall(rf"^          {re.escape(unset_commands)}$", canary, re.MULTILINE)) != 3:
        raise WorkflowContractError("external canary execution must unset command-file paths")
    if "actions/cache@" in canary or "Swatinem/rust-cache@" in canary:
        raise WorkflowContractError("upstream canary must not create source-derived caches")
    if canary.count("uses: mlugg/setup-zig@") != 1 or len(
        re.findall(r"^          use-cache: false$", canary, re.MULTILINE)
    ) != 1:
        raise WorkflowContractError("upstream canary Zig distribution cache is not bounded")
    if not all(
        fragment in canary
        for fragment in (
            "timeout-minutes: 5",
            "timeout-minutes: 45",
            "timeout 30s git",
            "python3 tools/check_herdr_api_contract.py",
            "python3 tools/live_product_smoke.py",
            'if [[ "$actual_zig" != "$HERDR_ZIG_VERSION" ]]; then',
            "Threat model: Herdr master is trusted against intentional runner escape.",
            "Result: \\`${{ job.status }}\\`",
        )
    ) or any(
        len(
            re.findall(
                rf'^          export ZIG_{kind}_CACHE_DIR="\$UPSTREAM_DIR/\.zig-cache"$',
                canary,
                re.MULTILINE,
            )
        )
        != 1
        for kind in ("GLOBAL", "LOCAL")
    ) or canary.count('test "$(git rev-parse HEAD)" = "$GITHUB_SHA"') != 2 or canary.count(
        "git diff --quiet HEAD --"
    ) != 2 or canary.count('test -z "$(git ls-files --others --exclude-standard)"') != 1 or canary.count(
        'test -z "$(git ls-files --others --ignored --exclude-standard)"'
    ) != 1:
        raise WorkflowContractError("upstream canary gates or reporting are incomplete")


def validate_release_publish_boundaries(workflows: dict[str, str]) -> None:
    release = workflows[PUBLISH_WORKFLOW]
    jobs = workflow_job_names(release)
    if jobs[-1:] != [PUBLISH_JOB]:
        raise WorkflowContractError("publication must be the last release job")
    gates = [job for job in jobs if job != PUBLISH_JOB]
    if not gates:
        raise WorkflowContractError("publication must depend on at least one gate")
    publish = workflow_job_block(release, PUBLISH_JOB)
    needed = [
        line.strip()[2:]
        for line in indented_block(publish, "needs", 4).splitlines()
        if line.strip()
    ]
    if sorted(needed) != sorted(gates):
        raise WorkflowContractError(
            "publication does not depend on every other release job"
        )
    # The notes come from the checkout being published from, re-verified there,
    # rather than from an artifact another job could have substituted.
    if (
        '--notes-out "${RUNNER_TEMP}/notes.md"' not in publish
        or "tools/check_release.py" not in publish
        or "--release" not in publish
        or "actions/upload-artifact@" in release
        or "actions/download-artifact@" in release
    ):
        raise WorkflowContractError("release notes are not collected in the publishing checkout")
    if (
        'if [[ ! "$TAG" =~ ^v[0-9]+\\.[0-9]+\\.[0-9]+$ ]]; then' not in publish
        or "--verify-tag" not in publish
        or '--notes-file "$RUNNER_TEMP/notes.md"' not in publish
    ):
        raise WorkflowContractError("publication does not refuse an unverified or unexpected tag")
    if "set -euo pipefail" not in publish:
        raise WorkflowContractError("publication does not fail closed on a failed step")
    # `needs` only orders jobs; `if: always()` would run publication anyway, and
    # a tolerated failure in a gate would let a broken tag through.
    if re.search(r"^    if *:", publish, re.MULTILINE):
        raise WorkflowContractError("publication must not run on a condition")
    if "continue-on-error" in release:
        raise WorkflowContractError("a release gate must not tolerate its own failure")


def minimal_workflow(*, action: str, checkout_option: str = "persist-credentials: false") -> str:
    return f"""\
name: Fixture
on:
  pull_request:
permissions:
  contents: read
jobs:
  check:
    runs-on: ubuntu-24.04
    steps:
      - uses: {action}
        with:
          {checkout_option}
"""


def workflow_paths(directory: Path) -> list[Path]:
    return sorted(
        path
        for path in directory.iterdir()
        if path.is_file() and path.suffix in {".yml", ".yaml"}
    )


def workflow_job_block(text: str, job: str) -> str:
    lines = text.splitlines()
    marker = f"  {job}:"
    try:
        start = lines.index(marker)
    except ValueError as error:
        raise WorkflowContractError(f"missing workflow job {job}") from error
    end = len(lines)
    for index in range(start + 1, len(lines)):
        if re.fullmatch(r"  [A-Za-z0-9_-]+:", lines[index]):
            end = index
            break
    return "\n".join(lines[start:end])


class WorkflowSecurityTests(unittest.TestCase):
    def repository_workflows(self) -> dict[str, str]:
        return {
            path.name: path.read_text(encoding="utf-8")
            for path in workflow_paths(WORKFLOW_DIR)
        }

    def test_repository_workflows_satisfy_security_contract(self) -> None:
        workflows = self.repository_workflows()
        self.assertTrue(workflows)
        for name, text in workflows.items():
            with self.subTest(name=name):
                validate_workflow(name, text)
        validate_live_product_boundaries(workflows)
        validate_upstream_canary_boundaries(workflows)
        validate_release_publish_boundaries(workflows)

    def test_release_publication_boundary_fails_closed(self) -> None:
        baseline = self.repository_workflows()
        mutations = []
        for old, new in (
            ("      - msrv\n", ""),
            ("      - gates\n", ""),
            ("      - identity\n", ""),
            ("--verify-tag", "--target main"),
            ("          set -euo pipefail\n          if [[ ! \"$TAG\"", "          if [[ ! \"$TAG\""),
            (
                'if [[ ! "$TAG" =~ ^v[0-9]+\\.[0-9]+\\.[0-9]+$ ]]; then',
                'if [[ -z "$TAG" ]]; then',
            ),
            (
                '--notes-out "${RUNNER_TEMP}/notes.md"',
                '--notes-out "${RUNNER_TEMP}/other.md"',
            ),
            (
                "  publish:\n    name: publish the GitHub release\n",
                "  publish:\n    name: publish the GitHub release\n    if: always()\n",
            ),
            (
                "  msrv:\n    name: rust-1.88 release\n",
                "  msrv:\n    name: rust-1.88 release\n    continue-on-error: true\n",
            ),
        ):
            changed = dict(baseline)
            changed[PUBLISH_WORKFLOW] = changed[PUBLISH_WORKFLOW].replace(old, new, 1)
            self.assertNotEqual(changed[PUBLISH_WORKFLOW], baseline[PUBLISH_WORKFLOW])
            mutations.append(changed)

        carried = dict(baseline)
        carried[PUBLISH_WORKFLOW] += (
            "      - uses: actions/download-artifact@"
            + "a" * 40
            + "\n        with:\n          name: notes\n"
        )
        mutations.append(carried)

        reordered = dict(baseline)
        publish = workflow_job_block(reordered[PUBLISH_WORKFLOW], PUBLISH_JOB)
        reordered[PUBLISH_WORKFLOW] = reordered[PUBLISH_WORKFLOW].replace(
            publish, ""
        ).replace("jobs:\n", f"jobs:\n{publish}\n", 1)
        mutations.append(reordered)

        for workflows in mutations:
            with self.subTest(workflows=workflows), self.assertRaises(
                WorkflowContractError
            ):
                validate_release_publish_boundaries(workflows)

    def test_only_the_publishing_job_may_hold_write_access(self) -> None:
        baseline = self.repository_workflows()
        for name in baseline:
            with self.subTest(name=name):
                validate_scoped_write_permissions(name, baseline[name])

        widened = baseline[PUBLISH_WORKFLOW].replace(
            "      contents: write", "      contents: write\n      id-token: write", 1
        )
        second = baseline[PUBLISH_WORKFLOW].replace(
            "  msrv:\n    name: rust-1.88 release\n",
            "  msrv:\n    name: rust-1.88 release\n    permissions:\n      contents: write\n",
            1,
        )
        moved = baseline[PUBLISH_WORKFLOW].replace(
            "    permissions:\n      contents: write\n", "", 1
        )
        for text in (widened, second, moved):
            with self.subTest(text=text), self.assertRaises(WorkflowContractError):
                validate_scoped_write_permissions(PUBLISH_WORKFLOW, text)

        elsewhere = baseline["ci.yml"].replace(
            "jobs:\n", "jobs:\n  leak:\n    permissions:\n      contents: write\n", 1
        )
        with self.assertRaisesRegex(WorkflowContractError, "below the workflow level"):
            validate_scoped_write_permissions("ci.yml", elsewhere)

    def test_live_product_secret_boundary_fails_closed(self) -> None:
        baseline = self.repository_workflows()
        mutations = []

        environment = dict(baseline)
        environment["live-product-core.yml"] += "\nenvironment: production\n"
        mutations.append(environment)

        mismatched = dict(baseline)
        mismatched["live-product-trusted.yml"] = mismatched[
            "live-product-trusted.yml"
        ].replace(
            "secrets.TETHER_SMOKE_REMOTE_TARGET",
            "secrets.TETHER_SMOKE_REMOTE_DIRECTORY",
            1,
        )
        mutations.append(mismatched)

        branch_gate = dict(baseline)
        branch_gate["live-product-trusted.yml"] = branch_gate[
            "live-product-trusted.yml"
        ].replace(
            "github.ref == 'refs/heads/main'",
            "github.ref != 'refs/heads/main'",
            1,
        )
        mutations.append(branch_gate)

        broadened_gate = dict(baseline)
        broadened_gate["live-product-trusted.yml"] = broadened_gate[
            "live-product-trusted.yml"
        ].replace(
            "github.ref == 'refs/heads/main'",
            "github.ref == 'refs/heads/main' || always()",
            1,
        )
        mutations.append(broadened_gate)

        required = dict(baseline)
        required["live-product-core.yml"] = required["live-product-core.yml"].replace(
            "required: false", "required: true", 1
        )
        mutations.append(required)

        for workflows in mutations:
            with self.subTest(workflows=workflows), self.assertRaises(
                WorkflowContractError
            ):
                validate_live_product_boundaries(workflows)

    def test_upstream_canary_boundary_fails_closed(self) -> None:
        baseline = self.repository_workflows()
        mutations = []
        for old, new in (
            ('cron: "23 5 * * *"', 'cron: "0 * * * *"'),
            ("  workflow_dispatch:", "  pull_request:"),
            ("  workflow_dispatch:", "  pull_request :"),
            ("--depth=1", "--depth=2"),
            ("checkout --quiet --detach", "checkout --quiet"),
            ("use-cache: false", "use-cache: true"),
            ("          use-cache: false", "          # use-cache: false"),
            ('          ACTIONS_RUNTIME_TOKEN: ""', '          ACTIONS_RUNTIME_TOKEN: "inherited"'),
            (
                "unset GITHUB_ENV GITHUB_OUTPUT GITHUB_PATH GITHUB_STATE GITHUB_STEP_SUMMARY",
                "true # command paths inherited",
            ),
            (
                "          unset GITHUB_ENV GITHUB_OUTPUT GITHUB_PATH GITHUB_STATE GITHUB_STEP_SUMMARY",
                "          # unset GITHUB_ENV GITHUB_OUTPUT GITHUB_PATH GITHUB_STATE GITHUB_STEP_SUMMARY",
            ),
            (
                '          export ZIG_GLOBAL_CACHE_DIR="$UPSTREAM_DIR/.zig-cache"',
                '          # export ZIG_GLOBAL_CACHE_DIR="$UPSTREAM_DIR/.zig-cache"',
            ),
            (
                '          export ZIG_LOCAL_CACHE_DIR="$UPSTREAM_DIR/.zig-cache"',
                '          export ZIG_GLOBAL_CACHE_DIR="$UPSTREAM_DIR/.zig-cache"',
            ),
            (
                'test -z "$(git ls-files --others --exclude-standard)"',
                "true # nonignored untracked files ignored",
            ),
            (
                'test -z "$(git ls-files --others --ignored --exclude-standard)"',
                "true # ignored untracked files ignored",
            ),
        ):
            changed = dict(baseline)
            changed["upstream-canary.yml"] = changed["upstream-canary.yml"].replace(
                old, new, 1
            )
            mutations.append(changed)
        with_secret = dict(baseline)
        with_secret["upstream-canary.yml"] += "\nenv:\n  TOKEN: ${{ secrets.VALUE }}\n"
        mutations.append(with_secret)

        for workflows in mutations:
            with self.subTest(workflows=workflows), self.assertRaises(
                WorkflowContractError
            ):
                validate_upstream_canary_boundaries(workflows)

    def test_external_actions_require_immutable_commit_pins(self) -> None:
        for action in ("actions/checkout@v4", "owner/action@main", "owner/action@abc123"):
            with self.subTest(action=action), self.assertRaisesRegex(
                WorkflowContractError, "full commit SHA"
            ):
                validate_workflow("fixture.yml", minimal_workflow(action=action))

        pinned = "actions/checkout@" + "a" * 40
        canonical = f"      - uses: {pinned}"
        for entry in (
            f"      - {{ uses: {pinned} }}",
            f"      - uses : {pinned}",
        ):
            with self.subTest(entry=entry), self.assertRaises(WorkflowContractError):
                validate_workflow(
                    "fixture.yml",
                    minimal_workflow(action=pinned).replace(canonical, entry),
                )

    def test_every_checkout_disables_persisted_credentials(self) -> None:
        action = "actions/checkout@" + "a" * 40
        for option in ("fetch-depth: 0", "persist-credentials: true"):
            with self.subTest(option=option), self.assertRaisesRegex(
                WorkflowContractError, "persist-credentials: false"
            ):
                validate_workflow(
                    "fixture.yml", minimal_workflow(action=action, checkout_option=option)
                )

    def test_pull_request_workflows_cannot_access_or_pass_secrets(self) -> None:
        action = "actions/checkout@" + "a" * 40
        baseline = minimal_workflow(action=action)
        mutations = (
            "env:\n  VALUE: ${{ secrets.VALUE }}\n",
            "env:\n  VALUE: ${{ secrets['VALUE'] }}\n",
            "env:\n  VALUE: ${{ toJSON(secrets) }}\n",
            "secrets:\n  VALUE: ${{ secrets.VALUE }}\n",
            "secrets:\n  inherit\n",
        )
        for mutation in mutations:
            with self.subTest(mutation=mutation), self.assertRaisesRegex(
                WorkflowContractError, "must not reference or pass secrets"
            ):
                validate_workflow("fixture.yml", baseline + mutation)

    def test_permissions_and_pull_request_target_fail_closed(self) -> None:
        action = "actions/checkout@" + "a" * 40
        baseline = minimal_workflow(action=action)
        invalid = (
            baseline.replace("contents: read", "contents: write"),
            baseline.replace("permissions:\n  contents: read", "permissions: write-all"),
            baseline.replace("pull_request:", "pull_request_target:"),
            baseline.replace(
                "    runs-on: ubuntu-24.04",
                "    permissions: write-all\n    runs-on: ubuntu-24.04",
            ),
        )
        for workflow in invalid:
            with self.subTest(workflow=workflow), self.assertRaises(WorkflowContractError):
                validate_workflow("fixture.yml", workflow)

    def test_quoted_security_keys_fail_closed(self) -> None:
        action = "actions/checkout@" + "a" * 40
        baseline = minimal_workflow(action=action)
        invalid = (
            baseline.replace("permissions:", '"permissions":'),
            baseline.replace("pull_request:", '"pull_request":'),
            baseline.replace(
                f"- uses: {action}",
                f'- "uses": {action}',
            ),
            baseline.replace(
                f"- uses: {action}",
                f'- "u\\x73es": {action}',
            ),
        )
        for workflow in invalid:
            with self.subTest(workflow=workflow), self.assertRaisesRegex(
                WorkflowContractError, "quoted mapping key"
            ):
                validate_workflow("fixture.yml", workflow)

    def test_flow_style_mappings_fail_closed(self) -> None:
        action = "actions/checkout@" + "a" * 40
        workflow = minimal_workflow(action=action).replace(
            "  check:\n    runs-on: ubuntu-24.04\n    steps:",
            "  check: { permissions: write-all, runs-on: ubuntu-24.04, steps: [] }",
        )
        with self.assertRaisesRegex(WorkflowContractError, "flow-style mapping"):
            validate_workflow("fixture.yml", workflow)

    def test_both_workflow_suffixes_are_discovered(self) -> None:
        import tempfile

        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for name in ("one.yml", "two.yaml", "ignored.txt"):
                (root / name).write_text("fixture", encoding="utf-8")
            self.assertEqual(
                [path.name for path in workflow_paths(root)],
                ["one.yml", "two.yaml"],
            )


if __name__ == "__main__":
    unittest.main()
