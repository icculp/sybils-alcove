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


if __name__ == "__main__":
    unittest.main()
