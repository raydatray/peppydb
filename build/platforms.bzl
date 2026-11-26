"""Execution platforms: local host, Miri mode, and opt-in BuildBuddy remote cache.

Buck2 builds procedural macros and build scripts for an execution platform,
not for the target platform. Under `@build/mode/miri` the Rust toolchain is a
pinned nightly, and a procedural macro must be built by the compiler that
loads it. So the Miri mode needs an execution platform that also carries
`mode-miri`: the Rust toolchain requires it there (`exec_compatible_with`),
and its exec dependencies are then configured with the nightly too.

Other modes require nothing of the execution platform and get the first one
registered, the plain host, as with the prelude's default.

When `remote_cache = True` (selected via `build/config/buildbuddy.bcfg`), the
execution platform still runs every action locally, but it reads action
results from BuildBuddy's remote cache and uploads the results of local
actions there (plan 0036). Remote execution is off: no action runs on a
BuildBuddy executor.
"""

load("@prelude//cfg/exec_platform:marker.bzl", "get_exec_platform_marker")

def _platform_impl(ctx: AnalysisContext) -> list[Provider]:
    constraints = dict()
    for setting in ctx.attrs.constraints:
        constraints.update(setting[ConfigurationInfo].constraints)
    cfg = ConfigurationInfo(constraints = constraints, values = {})
    label = ctx.label.raw_target()
    remote_cache = ctx.attrs.remote_cache
    executor_config = CommandExecutorConfig(
        local_enabled = True,
        remote_enabled = False,
        remote_cache_enabled = remote_cache,
        allow_cache_uploads = remote_cache,
        # Part of every action's cache key, so all machines that share the
        # cache must use the same values.
        remote_execution_properties = ctx.attrs.remote_execution_properties if remote_cache else None,
        remote_execution_use_case = "peppydb" if remote_cache else None,
        remote_output_paths = "output_paths" if remote_cache else None,
        use_windows_path_separators = host_info().os.is_windows,
    )
    return [
        DefaultInfo(),
        ExecutionPlatformInfo(
            label = label,
            configuration = cfg,
            executor_config = executor_config,
        ),
        PlatformInfo(label = str(label), configuration = cfg),
    ]

# One execution platform with the union of `constraints`. Actions always run
# locally; `remote_cache = True` adds BuildBuddy's remote cache.
peppy_execution_platform = rule(
    impl = _platform_impl,
    attrs = {
        "constraints": attrs.list(attrs.dep(providers = [ConfigurationInfo])),
        "remote_cache": attrs.bool(default = False),
        "remote_execution_properties": attrs.dict(attrs.string(), attrs.string(), default = {}),
    },
)

def _platforms_impl(ctx: AnalysisContext) -> list[Provider]:
    return [
        DefaultInfo(),
        ExecutionPlatformRegistrationInfo(
            platforms = [p[ExecutionPlatformInfo] for p in ctx.attrs.platforms],
            exec_marker_constraint = get_exec_platform_marker(),
        ),
    ]

# The execution platforms, in order of preference.
peppy_execution_platforms = rule(
    impl = _platforms_impl,
    attrs = {
        "platforms": attrs.list(attrs.dep(providers = [ExecutionPlatformInfo])),
    },
)
