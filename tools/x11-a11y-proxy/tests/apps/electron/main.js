// Electron probe: same controls as the other probes, in HTML; renderer console lines reach stdout through
// ELECTRON_ENABLE_LOGGING=1. Title from argv (--tag=...).
const { app, BrowserWindow, nativeTheme, screen } = require('electron');
const tag = (process.argv.find(a => a.startsWith('--tag=')) || '--tag=x').slice(6);
app.whenReady().then(() => {
  const w = new BrowserWindow({ width: 360, height: 260, title: 'Electron probe ' + tag });
  w.loadFile(__dirname + '/index.html');
  w.on('page-title-updated', e => e.preventDefault());
  const dump = () => console.log('SETTINGS electron', JSON.stringify({ a11y: app.isAccessibilitySupportEnabled(),
      dark: nativeTheme.shouldUseDarkColors, hc: nativeTheme.shouldUseHighContrastColors,
      invertedHC: nativeTheme.shouldUseInvertedColorScheme, zoom: w.webContents.getZoomFactor(),
      scale: screen.getPrimaryDisplay().scaleFactor }));
  setTimeout(() => { dump(); console.log('READY'); }, 3000);
  setInterval(dump, 4000);
});
