import { ref, computed } from "vue";
import { defineStore } from "pinia";
import { useI18n } from "vue-i18n";
import { useToast } from "@/composables/useToast";
import { isTauriRuntime } from "@/lib/backend/tauriRuntime";
import { safeLocalStorageGet, safeLocalStorageSet } from "@/lib/backend/safeStorage";
import * as api from "@/lib/backend/api";
import type { SqlFileEntry } from "@/lib/backend/api";
import { useConnectionStore } from "@/stores/connectionStore";
import { useQueryStore } from "@/stores/queryStore";
import { resolveDefaultDatabase } from "@/lib/database/defaultDatabase";

const STORAGE_KEY = "dbx-sql-file-folders";

export interface FolderState {
  path: string;
  entries: SqlFileEntry[];
  expanded: Set<string>;
  loading: boolean;
  collapsed: boolean;
}

export const useSqlFileStore = defineStore("sqlFile", () => {
  const { t } = useI18n();
  const { toast } = useToast();

  const folders = ref<FolderState[]>([]);
  let initialized = false;

  function loadSavedFolders(): string[] {
    try {
      const raw = safeLocalStorageGet(STORAGE_KEY);
      if (!raw) return [];
      const parsed = JSON.parse(raw);
      return Array.isArray(parsed) ? parsed.filter((p): p is string => typeof p === "string") : [];
    } catch {
      return [];
    }
  }

  function saveFolders() {
    const paths = folders.value.map((f) => f.path);
    safeLocalStorageSet(STORAGE_KEY, JSON.stringify(paths));
  }

  async function addFolder(folderPath: string) {
    if (folders.value.some((f) => f.path === folderPath)) return;
    folders.value.push({
      path: folderPath,
      entries: [],
      expanded: new Set(),
      loading: true,
      collapsed: false,
    });
    saveFolders();
    await loadFolderEntries(folderPath);
  }

  // Re-scan a single top-level folder and replace its entries. Mutated via the
  // reactive proxy (folders.value[idx]) so Vue tracks the change.
  async function loadFolderEntries(folderPath: string) {
    const idx = folders.value.findIndex((f) => f.path === folderPath);
    if (idx === -1) return;
    folders.value[idx].loading = true;
    try {
      const entries = await api.listSqlFilesInFolder(folderPath);
      const target = folders.value.findIndex((f) => f.path === folderPath);
      if (target !== -1) {
        folders.value[target].entries = entries;
        // Drop expand state for paths that no longer exist after the refresh so
        // stale entries don't keep phantom directories open.
        const stillPresent = new Set<string>();
        collectPaths(entries, stillPresent);
        const nextExpanded = new Set<string>();
        for (const p of folders.value[target].expanded) {
          if (stillPresent.has(p)) nextExpanded.add(p);
        }
        folders.value[target].expanded = nextExpanded;
      }
    } catch (e: any) {
      toast(t("sqlFileTree.loadFailed", { message: e?.message || String(e) }), 5000);
    } finally {
      const target = folders.value.findIndex((f) => f.path === folderPath);
      if (target !== -1) {
        folders.value[target].loading = false;
      }
    }
  }

  function collectPaths(entries: SqlFileEntry[], into: Set<string>) {
    for (const e of entries) {
      into.add(e.path);
      if (e.is_dir && e.children.length) collectPaths(e.children, into);
    }
  }

  async function refreshFolder(folderPath: string) {
    await loadFolderEntries(folderPath);
    toast(t("sqlFileTree.refreshed"), 1500);
  }

  async function refreshAll() {
    await Promise.all(folders.value.map((f) => loadFolderEntries(f.path)));
    toast(t("sqlFileTree.refreshed"), 1500);
  }

  // Open a SQL file from the SQL Files file explorer (opened folders) in a query
  // tab bound to the currently active connection. Mirrors SqlFilePanel.openFile.
  async function openFile(path: string) {
    if (!isTauriRuntime()) return;
    try {
      const content = await api.readExternalSqlFile(path);
      const connectionStore = useConnectionStore();
      const conn = connectionStore.connections.find((c) => c.id === connectionStore.activeConnectionId);
      const connectionId = conn?.id || "";
      const database = conn ? resolveDefaultDatabase(conn, []) : "";
      const queryStore = useQueryStore();
      queryStore.openExternalSqlFile(connectionId, database, path, content);
    } catch (err) {
      toast(`无法读取文件: ${err}`, 5000);
    }
  }

  async function removeFolder(index: number) {
    folders.value.splice(index, 1);
    saveFolders();
  }

  // Load the previously opened folders (persisted in localStorage) into the store.
  // Idempotent: safe to call from both app startup and the panel's onMounted.
  async function initFromStorage() {
    if (initialized) return;
    initialized = true;
    if (!isTauriRuntime()) return;
    const saved = loadSavedFolders();
    for (const path of saved) {
      await addFolder(path);
    }
  }

  // Flat list of every file (non-directory) entry across all opened folders.
  // Consumed by Quick Open to search the SQL Files file explorer.
  const allFileEntries = computed<SqlFileEntry[]>(() => {
    const out: SqlFileEntry[] = [];
    const walk = (entries: SqlFileEntry[]) => {
      for (const e of entries) {
        if (e.is_dir) walk(e.children);
        else out.push(e);
      }
    };
    for (const f of folders.value) walk(f.entries);
    return out;
  });

  return {
    folders,
    addFolder,
    loadFolderEntries,
    refreshFolder,
    refreshAll,
    openFile,
    removeFolder,
    initFromStorage,
    allFileEntries,
  };
});
