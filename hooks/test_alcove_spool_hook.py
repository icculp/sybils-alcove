#!/usr/bin/env python3
import importlib.util
import pathlib
import unittest


PATH = pathlib.Path(__file__).with_name("alcove_spool_hook.py")
SPEC = importlib.util.spec_from_file_location("alcove_spool_hook", PATH)
HOOK = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(HOOK)


class AgentLaunchClassificationTests(unittest.TestCase):
    def test_custom_exec_finds_wrapped_spark(self):
        source = '''const r = await tools.exec_command({
          cmd: "/root/bin/codex-spark-triage 'bounded task'",
          workdir: "/root"
        });'''
        self.assertEqual(HOOK._agent_launchers("exec", source), ["spark"])

    def test_search_that_mentions_launcher_is_not_a_launch(self):
        source = '''const r = await tools.exec_command({
          cmd: "rg -n 'codex-spark-triage|codex exec' /root/bin"
        });'''
        self.assertEqual(HOOK._agent_launchers("exec", source), [])

    def test_native_nested_agent_is_logged_without_prompt(self):
        launchers = HOOK._agent_launchers(
            "Agent",
            {"subagent_type": "Explore", "prompt": "must not be retained"},
        )
        self.assertEqual(launchers, ["Explore"])

    def test_patch_payload_with_command_example_is_not_a_launch(self):
        source = '''const patch = "*** Begin Patch\\n+cmd: \\\"/root/bin/codex-spark-triage task\\\"\\n*** End Patch";
        text(await tools.apply_patch(patch));'''
        self.assertEqual(HOOK._agent_launchers("exec", source), [])

    def test_direct_codex_exec_is_logged(self):
        self.assertEqual(
            HOOK._agent_launchers("Bash", {"command": "env FOO=1 codex exec 'task'"}),
            ["codex"],
        )

    def test_shell_wrapped_compound_launches_are_all_logged(self):
        self.assertEqual(
            HOOK._agent_launchers(
                "Bash",
                {"command": "bash -lc 'codex exec task; claude -p other'"},
            ),
            ["codex", "claude"],
        )


class SpawnParamTests(unittest.TestCase):
    """The tool_input shapes below are verbatim from captured payloads (with the
    prompt/message body elided), not read off a schema."""

    def test_claude_spawn_with_an_explicit_model(self):
        self.assertEqual(
            HOOK._spawn_params(
                "Agent",
                {
                    "description": "List files in hooks/ directory",
                    "prompt": "must not be retained",
                    "subagent_type": "Explore",
                    "model": "haiku",
                    "run_in_background": False,
                },
            ),
            {"model": "haiku", "subagent_type": "Explore", "run_in_background": False},
        )

    def test_claude_spawn_without_a_model_omits_the_key(self):
        # Verified live: the harness sends no `model` at all when the caller did
        # not choose one. Absent must not become a guessed default.
        params = HOOK._spawn_params(
            "Task",
            {"description": "d", "prompt": "p", "subagent_type": "Explore",
             "run_in_background": False},
        )
        self.assertNotIn("model", params)
        self.assertEqual(params, {"subagent_type": "Explore", "run_in_background": False})

    def test_codex_spawn_agent(self):
        self.assertEqual(
            HOOK._spawn_params(
                "spawn_agent",
                {
                    "agent_type": "default",
                    "fork_context": False,
                    "model": "gpt-5.5",
                    "reasoning_effort": "xhigh",
                    "service_tier": "priority",
                    "message": "must not be retained",
                },
            ),
            {"model": "gpt-5.5", "agent_type": "default", "reasoning_effort": "xhigh",
             "fork_context": False},
        )

    def test_a_non_spawn_tool_has_no_params(self):
        self.assertIsNone(HOOK._spawn_params("Bash", {"command": "ls", "model": "haiku"}))
        # `wait_agent` acts on an agent that already exists; only the spawn does.
        self.assertIsNone(HOOK._spawn_params("multi_agent_v1wait_agent", {"agent_id": "a"}))

    def test_a_spawn_that_named_nothing_is_absent_not_empty(self):
        self.assertIsNone(HOOK._spawn_params("Agent", {"prompt": "p", "description": "d"}))

    def test_the_prompt_is_never_a_param(self):
        for key in ("prompt", "message", "description", "content"):
            self.assertIsNone(HOOK._spawn_params("Agent", {key: "x" * 4000}))

    def test_an_over_long_value_set_is_capped_and_still_parsable(self):
        import json

        params = HOOK._spawn_params(
            "Agent",
            {"model": "m" * 400, "subagent_type": "s" * 400, "isolation": "worktree"},
        )
        blob = json.dumps(params, separators=(",", ":"))
        self.assertLessEqual(len(blob), HOOK.MAX_PARAMS)
        self.assertEqual(json.loads(blob), params, "a capped object is still an object")
        self.assertIn("model", params, "the cap drops the least load-bearing key, not the first")


if __name__ == "__main__":
    unittest.main()
