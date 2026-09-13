// Runs only in the main frame of the Windows app, before document scripts.
// This disposable color cache is independent of the device maintenance gate.
(() => {
  let appearance;
  try {
    const raw = localStorage.getItem("risunest.windowsAppearance");
    if (!raw) return;
    appearance = JSON.parse(raw);
    if (
      !appearance ||
      typeof appearance.dark !== "boolean" ||
      ![appearance.background, appearance.caption, appearance.text].every(
        (color) => typeof color === "string" && /^#[0-9a-f]{6}$/i.test(color),
      )
    ) {
      throw new Error("Invalid window color cache");
    }
  } catch {
    console.warn("Could not read the Windows startup colors");
    return;
  }
  const apply = () => {
    if (!document.documentElement) return false;
    const style = document.documentElement.style;
    style.setProperty("--risu-theme-bgcolor", appearance.background);
    style.setProperty("--risu-theme-darkbg", appearance.caption);
    style.setProperty("--risu-theme-textcolor", appearance.text);
    style.setProperty("--risu-theme-textcolor2", appearance.text);
    style.colorScheme = appearance.dark ? "dark" : "light";
    return true;
  };
  if (!apply()) {
    const observer = new MutationObserver(() => {
      if (apply()) observer.disconnect();
    });
    observer.observe(document, { childList: true });
  }
})();
