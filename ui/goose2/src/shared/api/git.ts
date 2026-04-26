import type {
  ChangedFile,
  CreatedWorktree,
  GitState,
} from "@/shared/types/git";
import { getClient } from "./acpConnection";

export async function getGitState(path: string): Promise<GitState> {
  const client = await getClient();
  const response = await client.goose.GooseGitState({ path });
  return response.state as GitState;
}

export async function switchBranch(
  path: string,
  branch: string,
): Promise<void> {
  const client = await getClient();
  await client.goose.GooseGitSwitchBranch({ path, branch });
}

export async function stashChanges(path: string): Promise<void> {
  const client = await getClient();
  await client.goose.GooseGitStash({ path });
}

export async function initRepo(path: string): Promise<void> {
  const client = await getClient();
  await client.goose.GooseGitInit({ path });
}

export async function fetchRepo(path: string): Promise<void> {
  const client = await getClient();
  await client.goose.GooseGitFetch({ path });
}

export async function pullRepo(path: string): Promise<void> {
  const client = await getClient();
  await client.goose.GooseGitPull({ path });
}

export async function createBranch(
  path: string,
  name: string,
  baseBranch: string,
): Promise<void> {
  const client = await getClient();
  await client.goose.GooseGitCreateBranch({ path, name, baseBranch });
}

export async function getChangedFiles(path: string): Promise<ChangedFile[]> {
  const client = await getClient();
  const response = await client.goose.GooseGitChangedFiles({ path });
  return response.files as ChangedFile[];
}

export async function createWorktree(
  path: string,
  name: string,
  branch: string,
  createBranch: boolean,
  baseBranch?: string,
): Promise<CreatedWorktree> {
  const client = await getClient();
  const response = await client.goose.GooseGitCreateWorktree({
    path,
    name,
    branch,
    createBranch,
    baseBranch,
  });
  return response.worktree as CreatedWorktree;
}
