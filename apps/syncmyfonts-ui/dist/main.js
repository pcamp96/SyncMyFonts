const invoke = window.__TAURI__?.core?.invoke;

const stepContent = {
  share: {
    progress: "14%",
    title: "Share fonts from this computer",
    copy: "Turn on LAN sharing on the computer that already has the font. SyncMyFonts will expose only user-installed fonts."
  },
  pair: {
    progress: "38%",
    title: "Pair with a nearby computer",
    copy: "Find a SyncMyFonts peer on the same trusted LAN, or paste the peer URL manually if discovery is blocked."
  },
  preview: {
    progress: "64%",
    title: "Preview before installing",
    copy: "Compare the missing fonts first. Nothing installs until you review the list and approve the transfer."
  },
  install: {
    progress: "100%",
    title: "Install missing fonts",
    copy: "Install the selected user fonts into this account, then reopen design apps if they do not appear immediately."
  }
};

const platformContent = {
  macos: {
    label: "macOS",
    title: "macOS",
    hint: "macOS user font folders are eligible. System font folders stay excluded.",
    copy: "Uses this Mac account's user font folders and keeps system font locations excluded."
  },
  windows: {
    label: "Windows",
    title: "Windows",
    hint: "Windows per-user font installs are eligible. Windows system fonts stay excluded.",
    copy: "Uses per-user font installs and keeps Windows system font locations excluded."
  }
};

function activePanel() {
  return document.querySelector(".view-panel.active");
}

function setText(id, value) {
  const element = document.getElementById(id);
  if (element) {
    element.textContent = value;
  }
}

function setToggle(id, enabled) {
  const element = document.getElementById(id);
  if (element) {
    element.classList.toggle("on", Boolean(enabled));
  }
}

function formatPeerCount(savedPeers, pairedPeers) {
  if (savedPeers === 0) {
    return "No saved peers";
  }
  if (savedPeers === pairedPeers) {
    return savedPeers === 1 ? "1 paired peer" : `${savedPeers} paired peers`;
  }
  return `${savedPeers} saved, ${pairedPeers} paired`;
}

function formatSharingState(sharing) {
  return sharing ? "On" : "Off";
}

function formatReadiness(warnings) {
  return Number(warnings) > 0 ? "Needs attention" : "Ready";
}

function formatWarningSummary(warnings) {
  const count = Number(warnings);
  if (count === 0) {
    return "No issues";
  }
  return count === 1 ? "1 issue" : `${count} issues`;
}

function renderPeerList(peers = []) {
  const list = document.getElementById("peerList");
  const empty = document.getElementById("peerEmptyState");
  if (!list || !empty) {
    return;
  }

  list.hidden = peers.length === 0;
  empty.hidden = peers.length > 0;
  list.replaceChildren(
    ...peers.map((peer) => {
      const row = document.createElement("div");
      row.className = "peer-row";
      const body = document.createElement("div");
      const name = document.createElement("strong");
      name.textContent = peer.name;
      const url = document.createElement("span");
      url.textContent = peer.url;
      body.append(name, url);
      const badge = document.createElement("span");
      badge.className = peer.paired ? "badge success" : "badge neutral";
      badge.textContent = peer.paired ? "Paired" : "Needs code";
      row.append(body, badge);
      return row;
    })
  );
}

function setView(viewName) {
  const panel = document.getElementById(`view-${viewName}`);
  if (!panel) {
    return;
  }

  document.querySelectorAll(".view-panel").forEach((view) => {
    view.classList.toggle("active", view === panel);
  });
  document.querySelectorAll(".nav-item").forEach((button) => {
    button.classList.toggle("active", button.dataset.view === viewName);
  });

  setText("viewTitle", panel.dataset.title ?? "SyncMyFonts");
  setText("viewEyebrow", panel.dataset.eyebrow ?? "Desktop app");
  setText("viewLede", panel.dataset.lede ?? "");
}

function updateStep(step) {
  const content = stepContent[step] ?? stepContent.share;
  setText("stepTitle", content.title);
  setText("stepCopy", content.copy);
  setText("flowStageTitle", content.title);
  document.querySelector(".sync-flow")?.style.setProperty("--flow-progress", content.progress);
  document.querySelectorAll(".flow-step").forEach((button) => {
    button.classList.toggle("active", button.dataset.step === step);
  });
}

function setPlatform(platformName) {
  const content = platformContent[platformName] ?? platformContent.macos;
  document.body.dataset.platform = platformName;
  setText("platformTitle", content.title);
  setText("platformCopy", content.copy);
  setText("platformPanelBadge", content.label);
  setText("platformHint", content.hint);
}

async function refreshSnapshot() {
  if (!invoke) {
    return;
  }

  try {
    const snapshot = await invoke("app_snapshot");
    setText("deviceName", snapshot.device_name);
    setText("stripDeviceName", snapshot.device_name);
    setText("localDeviceTitle", snapshot.device_name);
    setText("platformName", snapshot.platform === "windows" ? "Windows" : "macOS");
    setPlatform(snapshot.platform === "windows" ? "windows" : "macos");
    setText("sharingState", snapshot.sharing ? "Sharing on" : "Not sharing");
    setText("localShareStatus", snapshot.sharing ? "Sharing is on" : "Sharing is off");
    setText("peerCount", formatPeerCount(snapshot.saved_peers, snapshot.paired_peers));
    setText("stripSharingState", formatSharingState(snapshot.sharing));
    setText("stripPeerCount", String(snapshot.saved_peers));
    setText("warningSummary", formatWarningSummary(snapshot.warnings));
    setText("appReadiness", formatReadiness(snapshot.warnings));
    setText("userFontDir", snapshot.user_font_dir);
    setText("managedFontDir", snapshot.managed_font_dir);
    setText("userFontCount", `${snapshot.user_font_count} found`);
    setText("managedFontCount", `${snapshot.managed_manifest_count} managed`);
    setText("listenAddress", `Shares on ${snapshot.lan_listen_address} while this app is open. No port forwarding is required.`);
    setText(
      "systemFontPolicy",
      snapshot.system_fonts_excluded
        ? "System font folders are excluded from scan, share, and install workflows."
        : "System font exclusion needs attention before syncing."
    );
    setText(
      "autoSyncStatus",
      snapshot.auto_sync_saved_peers
        ? `On every ${snapshot.auto_sync_interval_minutes} minute(s) while the app is open.`
        : "Off until a saved peer is paired and you enable it."
    );
    setText("configPath", `Configuration file: ${snapshot.config_path}`);
    setText("logDir", `Log folder: ${snapshot.log_dir}`);
    setToggle("autoSyncToggle", snapshot.auto_sync_saved_peers);
    renderPeerList(snapshot.peers);
  } catch (error) {
    console.error("Unable to refresh SyncMyFonts snapshot", error);
  }
}

document.querySelectorAll(".flow-step").forEach((button) => {
  button.addEventListener("click", () => updateStep(button.dataset.step));
});

document.querySelectorAll(".nav-item").forEach((button) => {
  button.addEventListener("click", () => setView(button.dataset.view));
});

document.getElementById("refreshButton")?.addEventListener("click", refreshSnapshot);

setView(activePanel()?.id.replace("view-", "") ?? "sync");
setPlatform(document.body.dataset.platform ?? "macos");
updateStep("share");
refreshSnapshot();
