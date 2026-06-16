const invoke = window.__TAURI__?.core?.invoke;

const stepContent = {
  share: {
    title: "Share fonts from this computer",
    copy: "Turn on LAN sharing on the computer that already has the font. SyncMyFonts will expose only user-installed fonts."
  },
  pair: {
    title: "Pair with a nearby computer",
    copy: "Find a SyncMyFonts peer on the same trusted LAN, or paste the peer URL manually if discovery is blocked."
  },
  preview: {
    title: "Preview before installing",
    copy: "Compare the missing fonts first. Nothing installs until you review the list and approve the transfer."
  },
  install: {
    title: "Install missing fonts",
    copy: "Install the selected user fonts into this account, then reopen design apps if they do not appear immediately."
  }
};

function setText(id, value) {
  const element = document.getElementById(id);
  if (element) {
    element.textContent = value;
  }
}

function updateStep(step) {
  const content = stepContent[step] ?? stepContent.share;
  setText("stepTitle", content.title);
  setText("stepCopy", content.copy);
  document.querySelectorAll(".flow-step").forEach((button) => {
    button.classList.toggle("active", button.dataset.step === step);
  });
}

async function refreshSnapshot() {
  if (!invoke) {
    return;
  }

  try {
    const snapshot = await invoke("app_snapshot");
    setText("deviceName", snapshot.device_name);
    setText("localDeviceTitle", snapshot.device_name);
    setText("platformName", snapshot.platform);
    setText("sharingState", snapshot.sharing ? "sharing on" : "sharing off");
    setText("localShareStatus", snapshot.sharing ? "Sharing is on" : "Sharing is off");
    setText("peerCount", snapshot.saved_peers === 1 ? "1 saved peer" : `${snapshot.saved_peers} saved peers`);
    setText("warningCount", snapshot.warnings);
  } catch (error) {
    console.error("Unable to refresh SyncMyFonts snapshot", error);
  }
}

document.querySelectorAll(".flow-step").forEach((button) => {
  button.addEventListener("click", () => updateStep(button.dataset.step));
});

document.getElementById("refreshButton")?.addEventListener("click", refreshSnapshot);

updateStep("share");
refreshSnapshot();
