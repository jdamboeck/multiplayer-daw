export const meta = {
  name: 'review-fixes',
  description: 'Carry out docs/review-fixes-plan.md: each task implemented, then checked by a second agent, strictly one after the other',
  whenToUse: 'Run with args {stage: "all"}; pass skip: [...] to leave out tasks already in git.',
  phases: [
    { title: 'Preflight', detail: 'clean tree, right branch, plan present' },
    { title: 'Implement', detail: 'one agent per task, in plan order' },
    { title: 'Check', detail: 'a second agent re-runs the gates and reads the diff' },
    { title: 'Repair', detail: 'at most one round per task, then the stage stops' },
  ],
}

// ---- what a stage is ------------------------------------------------------------------------
//
// One stage: the plan needs no hand test. Everything runs one after the other.

const PLAN = 'docs/review-fixes-plan.md'

const TASKS = {
  T1: { heading: '## T1. The crash test waits for the guard, not for a clock' },
  T2: { heading: '## T2. The guard marks only the plugins a call really starts', hard: true },
  T3: { heading: '## T3. A normal exit with plugins off keeps the crash marker' },
  T4: { heading: '## T4. Parameter values come back from the store bit for bit' },
  T5: { heading: '## T5. A migration backup is whole or absent' },
  T6: { heading: '## T6. Small outbox repairs' },
}

const STAGES = {
  all: { tasks: ['T1', 'T2', 'T3', 'T4', 'T5', 'T6'] },
}

const input = args || {}
const stage = STAGES[input.stage]
if (!stage) {
  return { ok: false, reason: 'Pass args {stage: "all"}.' }
}
const skip = Array.isArray(input.skip) ? input.skip : []
const MODEL = input.model || 'opus'
const EFFORT = input.effort || 'medium'
const HARD_MODEL = input.hardModel || MODEL
const HARD_EFFORT = input.hardEffort || 'high'

// ---- what the agents are told ---------------------------------------------------------------

const GATES = [
  'cargo fmt --all -- --check',
  'cargo clippy --workspace --all-targets -- -D warnings',
  'cargo build -p mdaw-test-plugin',
  'env -u DISPLAY -u WAYLAND_DISPLAY cargo test --workspace',
].join('\n')

const RESULT = {
  type: 'object',
  properties: {
    status: {
      type: 'string',
      enum: ['done', 'done_without_commit', 'blocked'],
      description: 'done: committed. done_without_commit: the plan itself says this task may end without a commit, and it did. blocked: stopped as the plan\'s rules demand.',
    },
    commits: { type: 'array', items: { type: 'string' }, description: 'Hashes of the commits made, oldest first' },
    summary: { type: 'string', description: 'What was changed, in a few sentences' },
    measurements: { type: 'string', description: 'The failing output of each "Test first" step, run before the fix, and any numbers the task asked for; empty if none' },
    deviations: { type: 'array', items: { type: 'string' }, description: 'Every place where the work differs from the plan, and why' },
    blocker: { type: 'string', description: 'If blocked: what failed, with the output that shows it' },
  },
  required: ['status', 'commits', 'summary', 'deviations'],
}

const VERDICT = {
  type: 'object',
  properties: {
    pass: { type: 'boolean' },
    gates: { type: 'string', description: 'Which of the four gate commands passed and which failed, with the failing output' },
    findings: {
      type: 'array',
      items: {
        type: 'object',
        properties: {
          what: { type: 'string', description: 'What is wrong, with file and line' },
          planSays: { type: 'string', description: 'The sentence of the plan it goes against' },
        },
        required: ['what'],
      },
    },
  },
  required: ['pass', 'gates', 'findings'],
}

function where(task) {
  const t = TASKS[task]
  return t.within
    ? `the subsection headed "${t.heading}" inside "${t.within}" (read the introduction of that section too)`
    : `the section headed "${t.heading}"`
}

function implementPrompt(task) {
  const t = TASKS[task]
  return `You carry out one task of a written plan in this repository. You have not seen the plan before.

1. Read "${PLAN}" from the top down to the first "## T1." heading: the rules and the decisions. They bind you.
2. Read ${where(task)}, to the next heading of the same level. That is your whole task. Do not read ahead to do other tasks, and do not do work that belongs to them.
3. Run \`git status --short\` and \`git log --oneline -15\`. The tree must be clean. Earlier tasks of the plan are already committed; line numbers in the plan are from before them, so find code by the function names and the text the plan quotes.
4. Do the task exactly as written, step by step, in the order given. Where the plan gives code, use that code; adapt names only where the surrounding code has moved.
5. Run the gates, all four, and make them pass:

${GATES}

   The first cargo command may rebuild the native part under target/native; that can take many minutes. Never run tests that open windows on the real display.
6. ${t.docsOnly ? 'This task is documentation only.' : 'Check every "Done when" condition of the task, the grep commands included.'}
7. Commit only the files this task changed (\`git add\` them by name, never \`git add -A\`), with one plain sentence as the message, in the manner of \`git log --oneline\`. ${t.commits ? `This task makes ${t.commits}.` : ''} Never push.${t.mayNotCommit ? ' The plan says when this task ends without a commit: then leave the tree clean and report done_without_commit with the numbers.' : ''}

Where a step says "Test first", run the new test before the fix and put its failing output into "measurements". If it passes before the fix, stop and report "blocked" (the plan's rule 4).

When to stop instead: the plan's rules 2, 4 and 5. If a test fails that the plan does not say will change, or the task seems to need a file outside its "Files" list, or a step cannot be done as written: restore the tree to the last commit (\`git checkout -- . && git clean -fd\` for files you created), and report "blocked" with the exact output. Do not weaken an assertion, mark a test ignored, add a special case, or silence a warning to get through.

Report every place where what you did differs from the plan, however small.`
}

function checkPrompt(task, result) {
  const t = TASKS[task]
  return `You check one finished task of a written plan in this repository. Someone else did the work; you did not see it. Change nothing: no edits, no commits.

1. Read "${PLAN}" from the top down to the first "## T1." heading, then ${where(task)}.
2. The task's commits: ${result.commits.length ? result.commits.join(', ') : '(none reported)'}. Look at each with \`git show --stat\` and \`git show\`. \`git status --short\` must print nothing.
3. Check, and report each failure as a finding with file and line:
   - Only files the task's "Files" list allows were changed (plus what its steps name explicitly).
   - Nothing on the task's "Do not" list was done.
   - Every step of the task was carried out; where the plan gives code, the committed code does the same thing.
   - Every test the task asks for exists and tests what the plan says it tests.
   - No existing test was weakened: no assertion loosened or removed beyond what the plan names, no test newly marked #[ignore], no \`#[allow(...)]\` or leading underscore added to silence a warning.
   - Every "Done when" condition holds; run its grep commands.
   - Doc edits the task lists are there and say what is true of the code.
4. Run the gates yourself, all four. A gate that fails is a finding.

${GATES}

   The first cargo command may rebuild the native part; that can take many minutes. Never run tests that open windows on the real display.
5. The one who did the work reported these deviations from the plan: ${JSON.stringify(result.deviations)}. Judge each: acceptable (the plan's intent is kept) or a finding.

Pass only if there is no finding and all four gates pass. Style and taste are not findings; say nothing about them.`
}

function repairPrompt(task, verdict) {
  return `You repair one task of a written plan in this repository. The task was done and committed, and a check found faults. You have not seen the plan before.

1. Read "${PLAN}" from the top down to the first "## T1." heading, then ${where(task)}.
2. Look at the task's commit with \`git show\`.
3. The faults found:

${JSON.stringify(verdict.findings, null, 2)}

   The gates as the check saw them: ${verdict.gates}

4. Fix each fault as the plan says the task should have been done. Fix nothing else.
5. Run the gates, all four:

${GATES}

6. Put the fixes into the task's commit with \`git commit --amend --no-edit\` (nothing was pushed), adding files by name. Never push.

If a fault cannot be fixed without breaking a rule of the plan (see its rules 4 and 5), change nothing and report "blocked" with the reason.`
}

// ---- the run ---------------------------------------------------------------------------------

phase('Preflight')
const ready = await agent(
  `Check that this repository is ready for automated work. Change nothing.
- \`git status --short\` prints nothing (a clean tree).
- The file "${PLAN}" exists and is committed (\`git ls-files ${PLAN}\` prints it).
- \`git rev-parse --abbrev-ref HEAD\` is not "main".
- The folder target/native exists (the native build was made before).
Answer whether all hold, and if not, which does not and what the command printed.`,
  {
    label: 'preflight',
    effort: 'low',
    schema: {
      type: 'object',
      properties: { ok: { type: 'boolean' }, reason: { type: 'string' }, branch: { type: 'string' } },
      required: ['ok'],
    },
  },
)
if (!ready || !ready.ok) {
  return { ok: false, stage: input.stage, reason: ready ? ready.reason : 'the preflight agent did not answer' }
}

const done = []
for (const task of stage.tasks) {
  if (skip.includes(task)) {
    log(`${task}: skipped on request`)
    done.push({ task, status: 'skipped' })
    continue
  }
  const hard = TASKS[task].hard
  const opts = { model: hard ? HARD_MODEL : MODEL, effort: hard ? HARD_EFFORT : EFFORT }

  const result = await agent(implementPrompt(task), { ...opts, label: `do ${task}`, phase: 'Implement', schema: RESULT })
  if (!result || result.status === 'blocked') {
    log(`${task}: blocked; the stage stops here`)
    return {
      ok: false,
      stage: input.stage,
      stoppedAt: task,
      blocker: result
        ? result.blocker || `(no blocker field) ${result.summary}\n\nDeviations: ${JSON.stringify(result.deviations)}`
        : 'the agent ended without a result; check `git status` before going on',
      done,
      next: `Read the blocker, decide, then run this stage again: finished tasks are in git, so pass skip: ${JSON.stringify(done.map(d => d.task))} plus what you settled by hand.`,
    }
  }

  let verdict = await agent(checkPrompt(task, result), { ...opts, label: `check ${task}`, phase: 'Check', schema: VERDICT })
  let repaired = false
  if (verdict && !verdict.pass) {
    log(`${task}: the check found ${verdict.findings.length} fault(s); one repair round`)
    const repair = await agent(repairPrompt(task, verdict), { ...opts, label: `repair ${task}`, phase: 'Repair', schema: RESULT })
    repaired = true
    if (repair && repair.status !== 'blocked') {
      if (repair.commits && repair.commits.length) result.commits = repair.commits
      verdict = await agent(checkPrompt(task, result), { ...opts, label: `recheck ${task}`, phase: 'Check', schema: VERDICT })
    }
  }
  if (!verdict || !verdict.pass) {
    log(`${task}: not accepted; the stage stops here`)
    return {
      ok: false,
      stage: input.stage,
      stoppedAt: task,
      blocker: verdict ? { gates: verdict.gates, findings: verdict.findings } : 'the checking agent ended without a verdict',
      taskResult: result,
      done,
      next: `The task's commit is in git but was not accepted. Fix or revert it by hand, then run this stage again with skip: ${JSON.stringify(done.map(d => d.task))} (add "${task}" if you fixed it).`,
    }
  }
  log(`${task}: ${result.status}${repaired ? ' (after one repair)' : ''}`)
  done.push({
    task,
    status: result.status,
    commits: result.commits,
    summary: result.summary,
    measurements: result.measurements || '',
    deviations: result.deviations,
    repaired,
  })
}

return {
  ok: true,
  stage: input.stage,
  done,
  pause: 'All tasks of the plan are done. No hand test is needed; report per task to the user.',
}
