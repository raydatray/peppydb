"""Execution platforms: the host, and the host in Miri mode.

Buck2 builds procedural macros and build scripts for an execution platform,
not for the target platform. Under `@build/mode/miri` the Rust toolchain is a
pinned nightly, and a procedural macro must be built by the compiler that
loads it. So the Miri mode needs an execution platform that also carries
`mode-miri`: the Rust toolchain requires it there (`exec_compatible_with`),
and its exec dependencies are then configured with the nightly too.

Other modes require nothing of the execution platform and get the first one
registered, the plain host, as with the prelude's default.
"""

load("@prelude//cfg/exec_platform:marker.bzl", "get_exec_platform_marker")

def _platform_impl(ctx: AnalysisContext) -> list[Provider]:
    constraints = dict()
    for setting in ctx.attrs.constraints:
        constraints.update(setting[ConfigurationInfo].constraints)
    cfg = ConfigurationInfo(constraints = constraints, values = {})
    label = ctx.label.raw_target()
    return [
        DefaultInfo(),
        ExecutionPlatformInfo(
            label = label,
            configuration = cfg,
            executor_config = CommandExecutorConfig(
                local_enabled = True,
                remote_enabled = False,
                use_windows_path_separators = host_info().os.is_windows,
            ),
        ),
        PlatformInfo(label = str(label), configuration = cfg),
    ]

# One local execution platform with the union of `constraints`.
peppy_execution_platform = rule(
    impl = _platform_impl,
    attrs = {
        "constraints": attrs.list(attrs.dep(providers = [ConfigurationInfo])),
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
