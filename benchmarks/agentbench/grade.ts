#!/usr/bin/env bun
// Grading for agentbench tasks.
//
// `contains` and `regex` grade locally. The `judge` type is recognized, but its
// model-based grader is not implemented: `grade()` requires explicit
// `allowJudge` acknowledgement and then returns an ungraded result without
// spending tokens on another model call.

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
          `task ${task.id}: judge grading requires --allow-judge`,
        );
      }
      // Fail closed rather than spend money implicitly.
      return {
        success: null,
        reason: "judge grading is not implemented",
      };
    }
    default: {
      const exhaustive: never = type;
      throw new Error(`unknown success type: ${String(exhaustive)}`);
    }
  }
}
