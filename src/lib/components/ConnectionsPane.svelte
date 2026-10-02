<script lang="ts">
  import { toast } from "svelte-sonner";
  import appStore from "$lib/stores/app.svelte";
  import type { ConnectionInfo } from "$lib/api";
  import { hasFileDragPayload, readFileDragPayload } from "$lib/fileDrag";

  const engineLabel = (engine: string): string => {
    switch (engine.toLowerCase()) {
      case "mysql":
      case "mariadb":
        return "MySQL";
      case "postgres":
      case "postgresql":
        return "PostgreSQL";
      case "sqlite":
      case "sqlite3":
        return "SQLite";
      default:
        return engine;
    }
  };

  const isSqlite = (engine: string): boolean =>
    engine.toLowerCase() === "sqlite" || engine.toLowerCase() === "sqlite3";

  /// ツールチップに並べる詳細行を接続情報から組み立てる。
  /// 値が無い項目は省く。SSH トンネル情報は機密を含まない host/port/user のみ。
  const detailRows = (c: ConnectionInfo): { label: string; value: string }[] => {
    const rows: { label: string; value: string }[] = [];
    rows.push({ label: "Engine", value: engineLabel(c.engine) });
    if (isSqlite(c.engine)) {
      // sqlite は DB ファイルパスを schema に置くが、無ければ host に置いた
      // 設定も backend (db.rs の connect) がサポートする。同じフォールバックで表示する。
      const file = c.schema ?? c.host;
      if (file) rows.push({ label: "File", value: file });
    } else {
      if (c.host) {
        rows.push({
          label: "Host",
          value: c.port != null ? `${c.host}:${c.port}` : c.host,
        });
      } else if (c.port != null) {
        rows.push({ label: "Port", value: String(c.port) });
      }
      if (c.user) rows.push({ label: "User", value: c.user });
      // password_command の接続は「コマンドで取得する」ことだけを示す
      if (c.password_from_command) rows.push({ label: "Password", value: "(command)" });
      if (c.schema) rows.push({ label: "Database", value: c.schema });
    }
    if (c.ssh_tunnel) {
      const t = c.ssh_tunnel;
      const value = t.ssh_config
        ? `ssh_config: ${t.ssh_config}`
        : `${t.user}@${t.host}:${t.port}`;
      rows.push({ label: "SSH", value });
    }
    if (c.group_name) rows.push({ label: "Group", value: c.group_name });
    if (c.description) rows.push({ label: "Description", value: c.description });
    if (c.readonly) rows.push({ label: "Access", value: "read-only" });
    if (c.sql_ssl_mode) rows.push({ label: "TLS", value: c.sql_ssl_mode });
    return rows;
  };

  /// 設定順を保ったまま、連続する同一グループ名の接続をセクションにまとめる。
  /// group_name が無い接続はヘッダ無しのセクションになる。
  const sections = $derived.by(() => {
    const result: { group: string | null; items: ConnectionInfo[] }[] = [];
    for (const connection of appStore.connections) {
      const group = connection.group_name;
      const last = result[result.length - 1];
      if (last && last.group === group) {
        last.items.push(connection);
      } else {
        result.push({ group, items: [connection] });
      }
    }
    return result;
  });

  /// ホバー中の接続とアンカー (カーソル) 位置・ツールチップ表示位置 (viewport 座標)。
  let hovered = $state<ConnectionInfo | null>(null);
  let anchorX = $state(0);
  let anchorY = $state(0);
  let tipX = $state(0);
  let tipY = $state(0);
  /// レンダリング済みツールチップ要素 ($state にすることで bind:this 後に
  /// clamp の $effect が再実行され、実寸法で位置補正できる)。
  let tipEl = $state<HTMLDivElement | null>(null);

  const TIP_MARGIN = 16;

  /// クエリファイルをドラッグして重ねている接続名 (ドロップ先のハイライト用)
  let dropTarget = $state<string | null>(null);

  // FILES ペインからドラッグしてきたクエリファイルを受ける。
  // dragover では dataTransfer の中身を読めないので、種別 (MIME タイプ) だけで
  // 判定する。移動元と同じ接続には落とさせない (no-op なのでハイライトも出さない)。
  const canDrop = (e: DragEvent, connection: ConnectionInfo): boolean =>
    hasFileDragPayload(e.dataTransfer) &&
    appStore.selectedConnection !== connection.name;

  const handleDragOver = (e: DragEvent, connection: ConnectionInfo) => {
    if (!canDrop(e, connection)) {
      return;
    }
    // preventDefault しないとドロップが許可されない (HTML の DnD 仕様)
    e.preventDefault();
    if (e.dataTransfer) {
      e.dataTransfer.dropEffect = "move";
    }
    dropTarget = connection.name;
  };

  const handleDragLeave = (connection: ConnectionInfo) => {
    if (dropTarget === connection.name) {
      dropTarget = null;
    }
  };

  const handleDrop = async (e: DragEvent, connection: ConnectionInfo) => {
    dropTarget = null;
    if (!hasFileDragPayload(e.dataTransfer)) {
      return;
    }
    e.preventDefault();
    const payload = readFileDragPayload(e.dataTransfer);
    if (!payload || payload.connection === connection.name) {
      return;
    }
    // 移動元はドラッグ開始時の接続 (payload) を使う。ドロップまでの間に
    // 選択接続が変わっていても、掴んだファイルを取り違えない。
    const moved = await appStore.moveFileToConnection(
      payload.fileName,
      payload.connection,
      connection.name,
    );
    if (moved) {
      toast.success(`Moved "${payload.fileName}" to ${connection.name}`);
    } else {
      toast.error(`Failed to move "${payload.fileName}"`, {
        description: appStore.errorMessage ?? undefined,
      });
    }
  };

  const showTip = (c: ConnectionInfo, e: MouseEvent) => {
    hovered = c;
    setAnchor(e);
  };

  const setAnchor = (e: MouseEvent) => {
    anchorX = e.clientX;
    anchorY = e.clientY;
    // 実測クランプ ($effect) の前でもカーソル付近に出しておく (初回表示時に
    // 前回位置や左上へ一瞬ちらつくのを防ぐ)。溢れ補正は $effect が行う。
    tipX = e.clientX + TIP_MARGIN;
    tipY = e.clientY + TIP_MARGIN;
  };

  const hideTip = () => {
    hovered = null;
  };

  // アンカー位置とレンダリング済みツールチップの実寸法から、ビューポート内に
  // 収まる表示位置を決める。カーソルの右下に出し、右端・下端で溢れる場合は
  // 左側・上側へ反転させる。長い値 (SQLite パスや description) で折り返して
  // 高さ/幅が変わっても、実測サイズを使うので確実に画面内へ収まる。
  $effect(() => {
    // hovered / anchor / tipEl を依存に取る (tipEl 確定後に再実行される)
    if (!hovered || !tipEl) return;
    const w = tipEl.offsetWidth;
    const h = tipEl.offsetHeight;
    let x = anchorX + TIP_MARGIN;
    if (x + w > window.innerWidth) {
      x = Math.max(TIP_MARGIN, anchorX - TIP_MARGIN - w);
    }
    let y = anchorY + TIP_MARGIN;
    if (y + h > window.innerHeight) {
      y = Math.max(TIP_MARGIN, anchorY - TIP_MARGIN - h);
    }
    tipX = x;
    tipY = y;
  });
</script>

<div class="flex h-full w-full flex-col border-r border-zinc-700 bg-zinc-900">
  <div class="flex items-center justify-between border-b border-zinc-700 px-3 py-2">
    <span class="text-xs font-semibold tracking-wide text-zinc-400">CONNECTIONS</span>
    <button
      class="rounded px-1.5 py-0.5 text-xs text-zinc-400 hover:bg-zinc-700 hover:text-zinc-200"
      title="Reload connections"
      aria-label="Reload connections"
      data-annotate="button-reload-connections"
      onclick={() => appStore.reloadConnections()}
    >
      <i class="bi bi-arrow-clockwise" aria-hidden="true"></i>
    </button>
  </div>
  <div class="min-h-0 flex-1 overflow-y-auto">
    {#if appStore.loadingConnections}
      <p class="px-3 py-2 text-xs text-zinc-500">Loading...</p>
    {:else if appStore.connections.length === 0}
      <p class="px-3 py-2 text-xs text-zinc-500">
        No connections. Review your config file.
      </p>
    {:else}
      {#each sections as section, sectionIndex (sectionIndex)}
        {#if section.group !== null}
          <div
            class="sticky top-0 z-10 bg-zinc-700 px-3 py-1 text-[10px] font-semibold uppercase tracking-wide text-zinc-200"
            data-annotate="connection-group-{section.group}"
          >
            {section.group}
          </div>
        {/if}
        {#each section.items as connection (connection.name)}
          <button
            class="flex w-full flex-col gap-0.5 px-3 py-2 text-left hover:bg-zinc-800 {appStore.selectedConnection ===
            connection.name
              ? 'bg-zinc-800 border-l-2 border-blue-400'
              : 'border-l-2 border-transparent'} {dropTarget === connection.name
              ? 'bg-blue-500/20 outline outline-1 outline-blue-400'
              : ''}"
            data-annotate="button-connection-{connection.name}"
            onclick={() => appStore.selectConnection(connection.name)}
            onmouseenter={(e) => showTip(connection, e)}
            onmousemove={(e) => hovered && setAnchor(e)}
            onmouseleave={hideTip}
            ondragover={(e) => handleDragOver(e, connection)}
            ondragleave={() => handleDragLeave(connection)}
            ondrop={(e) => void handleDrop(e, connection)}
          >
            <span class="truncate text-sm text-zinc-200">{connection.name}</span>
            <span class="flex items-center gap-1 text-xs text-zinc-500">
              {engineLabel(connection.engine)}
              {#if connection.has_ssh_tunnel}
                <span
                  class="rounded bg-zinc-700 px-1 text-[10px] text-zinc-300"
                  title="Via SSH tunnel">ssh</span
                >
              {/if}
              {#if connection.readonly}
                <span
                  class="rounded bg-yellow-500/15 px-1 text-[10px] text-yellow-400"
                  title="Write statements are rejected (readonly: true in config)"
                  data-annotate="badge-readonly-{connection.name}">read-only</span
                >
              {/if}
              {#if connection.sql_ssl_mode}
                <!-- 画面には出さない。詳細ツールチップは mouseenter でしか開かない
                     ため、バッヂを消すとキーボード / スクリーンリーダーからは TLS の
                     状態を知る手段が無くなる。警告ではなく値そのものを読ませる。 -->
                <span class="sr-only">TLS: {connection.sql_ssl_mode}</span>
              {/if}
            </span>
            {#if connection.description}
              <span class="truncate text-xs text-zinc-500"
                >{connection.description}</span
              >
            {/if}
          </button>
        {/each}
      {/each}
    {/if}
  </div>
</div>

{#if hovered}
  <div
    bind:this={tipEl}
    class="pointer-events-none fixed z-50 max-w-sm rounded border border-zinc-600 bg-zinc-800 px-3 py-2 shadow-lg"
    style="left: {tipX}px; top: {tipY}px;"
    data-annotate="tooltip-connection-{hovered.name}"
    role="tooltip"
  >
    <div class="mb-1 text-sm font-semibold text-zinc-100 break-all">{hovered.name}</div>
    <dl class="grid grid-cols-[auto_1fr] gap-x-2 gap-y-0.5 text-xs">
      {#each detailRows(hovered) as row}
        <dt class="text-zinc-400">{row.label}</dt>
        <dd class="text-zinc-200 break-all whitespace-pre-wrap">{row.value}</dd>
      {/each}
    </dl>
  </div>
{/if}
