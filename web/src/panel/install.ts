// The command that connects a node, as the spec gives it: the pool serves `install.sh` itself.
export function installCommand(origin: string = window.location.origin): string {
  return `curl -fsSL ${origin}/install.sh | sudo env MTPOOL_TOKEN=<токен> sh -s -- --name <имя>`;
}

/** Copies text; false if the browser refuses. */
export async function copyText(text: string): Promise<boolean> {
  try {
    await navigator.clipboard.writeText(text);
    return true;
  } catch {
    return false;
  }
}
