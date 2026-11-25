import { create } from "zustand";
import { parseSettingsHash, settingsHash } from "./settingsTabs";

/**
 * Whether the settings dialog is open and on which tab, kept in the URL hash
 * (`#settings/<tab>`) so a tab can be linked and the browser's reload keeps it.
 */
interface SettingsNav {
  /** null: closed; "": open on the first tab the user sees. */
  tab: string | null;
  open: (tab?: string) => void;
  /** The dialog settled on a tab (shown or picked). */
  show: (tab: string) => void;
  close: () => void;
}

function writeHash(hash: string | null) {
  const url = location.pathname + location.search + (hash ?? "");
  if (url !== location.pathname + location.search + location.hash) history.replaceState(history.state, "", url);
}

export const useSettingsNav = create<SettingsNav>((set) => ({
  tab: parseSettingsHash(location.hash),
  open: (tab) => set({ tab: tab ?? "" }),
  show: (tab) => {
    writeHash(settingsHash(tab));
    set({ tab });
  },
  close: () => {
    if (parseSettingsHash(location.hash) !== null) writeHash(null);
    set({ tab: null });
  },
}));

/** Opens the settings, on a tab (Account, Users, ...) or the first one. */
export function openSettings(tab?: string) {
  useSettingsNav.getState().open(tab);
}

// A link to #settings/... while the app is open.
window.addEventListener("hashchange", () => {
  const tab = parseSettingsHash(location.hash);
  if (tab !== null) useSettingsNav.setState({ tab });
});
