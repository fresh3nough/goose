import { getClient } from "@/shared/api/acpConnection";

export interface FileTreeEntry {
  name: string;
  path: string;
  kind: "file" | "directory";
}

export interface AttachmentPathInfo {
  name: string;
  path: string;
  kind: "file" | "directory";
  mimeType?: string | null;
}

export interface ImageAttachmentPayload {
  base64: string;
  mimeType: string;
}

export async function getHomeDir(): Promise<string> {
  const client = await getClient();
  const response = await client.goose.GooseSystemHomeDir({});
  return response.path;
}

export async function pathExists(path: string): Promise<boolean> {
  const client = await getClient();
  const response = await client.goose.GooseSystemPathExists({ path });
  return response.exists;
}

export async function listDirectoryEntries(
  path: string,
): Promise<FileTreeEntry[]> {
  const client = await getClient();
  const response = await client.goose.GooseSystemListDirectoryEntries({ path });
  return response.entries.map((entry) => ({
    name: entry.name,
    path: entry.path,
    kind: entry.kind === "directory" ? "directory" : "file",
  }));
}

export async function inspectAttachmentPaths(
  paths: string[],
): Promise<AttachmentPathInfo[]> {
  const client = await getClient();
  const response = await client.goose.GooseSystemInspectAttachmentPaths({
    paths,
  });
  return response.attachments.map((attachment) => ({
    name: attachment.name,
    path: attachment.path,
    kind: attachment.kind === "directory" ? "directory" : "file",
    ...(attachment.mimeType ? { mimeType: attachment.mimeType } : {}),
  }));
}

export async function listFilesForMentions(
  roots: string[],
  maxResults = 1500,
): Promise<string[]> {
  const client = await getClient();
  const response = await client.goose.GooseSystemListFilesForMentions({
    roots,
    maxResults,
  });
  return response.files;
}

export async function readImageAttachment(
  path: string,
): Promise<ImageAttachmentPayload> {
  const client = await getClient();
  const response = await client.goose.GooseSystemReadImageAttachment({ path });
  return { base64: response.base64, mimeType: response.mimeType };
}

/**
 * Write a UTF-8 string to a path on disk, creating any missing parent
 * directories. The desktop shell uses this to persist content the user has
 * chosen via a native file dialog (e.g. exported session JSON).
 */
export async function writeFile(path: string, contents: string): Promise<void> {
  const client = await getClient();
  await client.goose.GooseSystemWriteFile({ path, contents });
}
