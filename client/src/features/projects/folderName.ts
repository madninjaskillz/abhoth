// The picker's "New folder" name check, the same rules as the server's (FolderBrowser.FolderNameError):
// one plain segment that both Windows and Linux accept. No imports, so plain node tests it.

const RESERVED = new Set([
  "CON", "PRN", "AUX", "NUL", "CONIN$", "CONOUT$",
  ..."123456789¹²³".split("").flatMap((d) => [`COM${d}`, `LPT${d}`]),
]);

/** Why `name` can't be a new folder's name, or null when it can. */
export function folderNameError(name: string): string | null {
  if (!name.trim()) return "Type a name for the new folder.";
  if (name === "." || name === "..") return "\".\" and \"..\" aren't folder names.";
  if (/[/\\]/.test(name)) return "A folder name can't contain / or \\.";
  for (const c of name) {
    const code = c.charCodeAt(0);
    if (code < 32 || code === 127) return "A folder name can't contain control characters.";
    if ('<>:"|?*'.includes(c)) return `A folder name can't contain ${c}.`;
  }
  if (name.endsWith(".") || name.endsWith(" ")) return "A folder name can't end with a dot or a space.";
  if (name.length > 255) return "That name is too long.";
  const stem = name.split(".")[0].replace(/ +$/, "");
  if (RESERVED.has(stem.toUpperCase())) return `"${stem.toUpperCase()}" is a reserved name on Windows.`;
  return null;
}
