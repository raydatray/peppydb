"""Execution platforms: local host, Miri mode, and opt-in BuildBuddy hybrid RBE.

Buck2 builds procedural macros and build scripts for an execution platform,
not for the target platform. Under `@build/mode/miri` the Rust toolchain is a
pinned nightly, and a procedural macro must be built by the compiler that
loads it. So the Miri mode needs an execution platform that also carries
`mode-miri`: the Rust toolchain requires it there (`exec_compatible_with`),
and its exec dependencies are then configured with the nightly too.

Other modes require nothing of the execution platform and get the first one
registered, the plain host, as with the prelude's default.

When `remote = True` (selected via `build/config/buildbuddy.bcfg`), the
execution platform enables Buck2's limited hybrid executor with BuildBuddy
remote caching and CAS uploads (plan 0036).
"""

load("@prelude//cfg/exec_platform:marker.bzl", "get_exec_platform_marker")

def _platform_impl(ctx: AnalysisContext) -> list[Provider]:
    constraints = dict()
    for setting in ctx.attrs.constraints:
        constraints.update(setting[ConfigurationInfo].constraints)
    cfg = ConfigurationInfo(constraints = constraints, values = {})
    label = ctx.label.raw_target()
    remote = ctx.attrs.remote
    executor_config = CommandExecutorConfig(
        local_enabled = True,
        remote_enabled = remote,
        remote_cache_enabled = remote,
        use_limited_hybrid = remote,
        allow_limited_hybrid_fallbacks = remote,
        allow_cache_uploads = remote,
        remote_execution_properties = ctx.attrs.remote_execution_properties if remote else None,
        remote_execution_use_case = "peppydb" if remote else None,
        remote_output_paths = "output_paths" if remote else None,
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

# One execution platform with the union of `constraints`, local-only by default
# or hybrid local+remote when `remote = True`.
peppy_execution_platform = rule(
    impl = _platform_impl,
    attrs = {
        "constraints": attrs.list(attrs.dep(providers = [ConfigurationInfo])),
        "remote": attrs.bool(default = False),
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
