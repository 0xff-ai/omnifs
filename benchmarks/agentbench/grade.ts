#!/usr/bin/env bun
// Grading for agentbench tasks.
//
// T3 supports `contains` and `regex` grading locally and for free. The `judge`
// type is recognized and parses, but grading a judge task requires an extra
// model call (cost), which is gated to T4: `grade()` refuses a judge task
// unless `allowJudge` is set, and even then reports it as unimplemented here so
// no model spend can happen inside this step.

export type SuccessType = "contains" | "regex" | "judge";

export interface TaskSuccess {
  type: SuccessType;
  value: string;
}

export interface Task {
  id: string;
  family: string;
  prompt: string;
  success: TaskSuccess;
  max_turns: number;
}

export interface GradeResult {
  success: boolean | null; // null = not gradeable in this build (judge)
  reason: string;
}

export interface GradeOptions {
  allowJudge?: boolean;
}

export function grade(
  task: Task,
  answer: string,
  opts: GradeOptions = {},
): GradeResult {
  const { type, value } = task.success;
  switch (type) {
    case "contains": {
      const hit = answer.toLowerCase().includes(value.toLowerCase());
      return {
        success: hit,
        reason: hit ? `contains "${value}"` : `missing "${value}"`,
      };
    }
    case "regex": {
      const re = new RegExp(value);
      const hit = re.test(answer);
      return {
        success: hit,
        reason: hit ? `regex /${value}/ matched` : `regex /${value}/ no match`,
      };
    }
    case "judge": {
      if (!opts.allowJudge) {
        throw new Error(
          `task ${task.id}: judge grading requires --allow-judge (gated to T4)`,
        );
      }
      // --allow-judge acknowledged, but the judge model call is a T4 deliverable.
      // Fail closed rather than spend money implicitly.
      return {
        success: null,
        reason: "judge grading is implemented in T4; not available in this build",
      };
    }
    default: {
      const exhaustive: never = type;
      throw new Error(`unknown success type: ${String(exhaustive)}`);
    }
  }
}
