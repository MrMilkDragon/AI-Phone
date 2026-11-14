import { create, fromBinary, toBinary } from "@bufbuild/protobuf";
import { deepEqual, equal, match, ok } from "node:assert/strict";
import { copyFileSync, existsSync, mkdirSync, readdirSync, readFileSync } from "node:fs";
import { join } from "node:path";
import { ControlStateSchema, DeviceActionRequestSchema, DeviceActionResponseSchema, TakeControlRequestSchema } from "../packages/protocol/src/gen/ai_phone/v1/phone_runtime_pb";
import type { MessageInitShape } from "@bufbuild/protobuf";
import { RegistryRequestSchema, RegistryStateSchema } from "../packages/protocol/src/gen/ai_phone/v1/registry_pb";

const root = process.env.AIMANITY_QA_ROOT;
if (!root || process.platform !== "win32") throw Error("Run on the disposable Windows acceptance host");
import {FileListingSchema, FileLocationSchema, FileManageRequestSchema, FileManageResultSchema, FileOperation} from "../packages/protocol/src/gen/ai_phone/v1/workbench_pb";
const started = performance.now();
const install = join(root, "installed");
const appData = join(root, "profile");
const home = join(appData, "dev.aiphone.client", "host");
const installer = join(root, process.env.AIMANITY_QA_BERYLLIUM_FRESH === "1" ? "AImanity-0.0.2-windows-x64-setup.exe" : "AImanity-0.0.1-windows-x64-setup.exe");
const installation = Bun.spawn([installer, "/VERYSILENT", "/SUPPRESSMSGBOXES", "/SP-",
  "/NORESTART", "/RESTARTEXITCODE=3010", `/DIR=${install}`,
], { stdout: "ignore", stderr: "inherit" });
const exit = await installation.exited;
if (exit === 3010) throw Error("Windows requires a reboot after enabling the Hypervisor Platform");
equal(exit, 0, "Inno Setup exit code");
console.log(JSON.stringify({ installerExit: exit, ms: Math.round(performance.now() - started) }));

const env = { ...process.env, APPDATA: appData, LOCALAPPDATA: appData,
  WEBVIEW2_USER_DATA_FOLDER: join(root, "webview"),
};
const program = join(install, "ai-phone-client.exe");
const app = Bun.spawn([program], { cwd: install, env, stdout: "ignore", stderr: "inherit" });
console.log("Started installed AImanity GUI process " + app.pid);
const origin = "http://127.0.0.1:17860";
let webReady = false;
const watchWeb = setInterval(async () => {
  if (webReady) return;
  const response = await fetch(`${origin}/phone`, { method: "HEAD", redirect: "manual", signal: AbortSignal.timeout(2400) }).catch(() => undefined);
  if (!response?.ok || !response.headers.get("content-type")?.startsWith("text/html")) return;
  webReady = true;
  console.log(`Installed Windows workbench HTTP: ${response.status} ${response.headers.get("content-type")} at ${Math.round(performance.now() - started)} ms`);
}, 4000);
const checkpoint = join(home, "deployment.json");
const androidRoot = join(home, "user", "data", "android");
const logPaths = ["user/qemu/owner.log", "user/qemu/boot.log", "user/qemu/qemu.log",
  "user/data/android/native/sdk.log", "user/data/android/whpx-avd/cuttlefish/kernel.log", "deployment.json"];
let androidStarted = 0;
let reportedAndroid = false;
let reportedListeners = false;
const watchAndroid = setInterval(() => {
  if (!existsSync(checkpoint) || !existsSync(join(androidRoot, "native", "sdk.log"))) return;
  if (!androidStarted) androidStarted = Date.now();
  const reportBoot = !reportedAndroid && Date.now() - androidStarted >= 85000;
  const reportListeners = reportedAndroid && !reportedListeners && Date.now() - androidStarted >= 114000;
  if (!reportBoot && !reportListeners) return;
  const components = join(home, "components", "android");
  if (!existsSync(components)) return;
  const [identity] = readdirSync(components);
  const adb = join(components, identity!, "sdk", "platform-tools", "adb.exe");
  const adbInfo = (...args: string[]) => Bun.spawnSync([adb, "-P", "15037", ...args], { env, timeout: 4000 }).stdout.toString().trim();
  if (reportBoot) {
    reportedAndroid = true;
    console.log(`Android boot transport: ${adbInfo("devices", "-l")}\n${adbInfo("-s", "127.0.0.1:5565", "shell", "getprop sys.boot_completed; id -u")}`);
  }
  if (reportListeners) {
    reportedListeners = true;
    const shell = (command: string) => adbInfo("-s", "127.0.0.1:5565", "shell", command);
    console.log(`Android first-run user state: ${shell("am get-started-user-state 0; date +%s")}; host clock ${Date.now()}`);
    for (const name of ["notifications", "attention"]) {
      const raw = shell(`cat /data/user/0/dev.aiphone.input/files/${name}.json 2>/dev/null`);
      if (!raw) { console.log(`Android ${name} listener has not written a receipt`); continue; }
      const receipt = JSON.parse(raw);
      console.log(`Android ${name} listener: connected=${receipt.connected}, checked_at_ms=${receipt.checked_at_ms}`);
    }
  }
}, 10000);

const automation = String.raw`
  $ErrorActionPreference = 'Stop'
  Add-Type -AssemblyName UIAutomationClient, UIAutomationTypes
  $checkpoint = Join-Path $env:APPDATA 'dev.aiphone.client\host\deployment.json'
  $element = [System.Windows.Automation.AutomationElement]
  $descendants = [System.Windows.Automation.TreeScope]::Descendants
  $buttonType = [System.Windows.Automation.PropertyCondition]::new($element::ControlTypeProperty, [System.Windows.Automation.ControlType]::Button)
  $enabled = [System.Windows.Automation.PropertyCondition]::new($element::IsEnabledProperty, $true)
  $visible = [System.Windows.Automation.PropertyCondition]::new($element::IsOffscreenProperty, $false)
  $lastPhase = ''

  function Find-Button($name, $seconds) {
    $named = [System.Windows.Automation.PropertyCondition]::new($element::NameProperty, $name)
    $selector = [System.Windows.Automation.AndCondition]::new([System.Windows.Automation.Condition[]] @($buttonType, $enabled, $visible, $named))
    $deadline = [DateTime]::UtcNow.AddSeconds($seconds)
    do {
      $process = Get-Process -Id $env:AIMANITY_QA_PID
      if ($process.MainWindowHandle) {
        $window = $element::FromHandle($process.MainWindowHandle)
        $button = $window.FindFirst($descendants, $selector)
        if ($button) { return $button }
      }
      if (Test-Path $checkpoint) {
        $progress = (Get-Content $checkpoint -Raw | ConvertFrom-Json).progress
        $phase = "$($progress.phase): $($progress.state)"
        if ($phase -ne $script:lastPhase) {
          Write-Host "Native setup: $phase"
          $script:lastPhase = $phase
        }
        if ($progress.state -in @('failed', 'blocked', 'interrupted')) {
          throw "Native setup $phase ($($progress.error))"
        }
      }
      Start-Sleep -Milliseconds 450
    } while ([DateTime]::UtcNow -lt $deadline)
    if ($window) {
      $window.FindAll($descendants, $buttonType) |
        ForEach-Object { 'Button: {0}; enabled={1}; offscreen={2}' -f $_.Current.Name, $_.Current.IsEnabled, $_.Current.IsOffscreen } |
        Select-Object -First 15 | Write-Host
    }
    throw "Installed window did not expose the enabled '$name' button."
  }

  function Invoke-Button($button) {
    $name = $button.Current.Name
    $action = $button.GetCurrentPattern([System.Windows.Automation.InvokePattern]::Pattern)
    $action.Invoke()
    Write-Host "Clicked installed client button: $name"
  }

  function Wait-WorkbenchHealth {
    Add-Type -AssemblyName System.Net.Http
    $handler = [System.Net.Http.HttpClientHandler]::new()
    $handler.UseProxy = $false
    $handler.AllowAutoRedirect = $false
    $client = [System.Net.Http.HttpClient]::new($handler)
    $client.Timeout = [TimeSpan]::FromSeconds(4)
    $deadline = [DateTime]::UtcNow.AddSeconds(35)
    $observed = ''
    try {
      do {
        $probe = [System.Net.Http.HttpRequestMessage]::new([System.Net.Http.HttpMethod]::Head, 'http://127.0.0.1:17860/phone')
        $response = $null
        try {
          $response = $client.SendAsync($probe).GetAwaiter().GetResult()
          $media = $response.Content.Headers.ContentType.MediaType
          $observed = "{0} {1}; redirect={2}" -f [int]$response.StatusCode, $media, $response.Headers.Location
          if ($response.IsSuccessStatusCode -and $media -eq 'text/html') {
            Write-Host "Pre-open workbench HEAD /phone: $observed"
            return
          }
        } catch {
          $observed = $_.Exception.GetBaseException().Message
        } finally {
          if ($response) { $response.Dispose() }
          $probe.Dispose()
        }
        Start-Sleep -Milliseconds 350
      } while ([DateTime]::UtcNow -lt $deadline)
      throw "Workbench HEAD /phone did not return HTML: $observed"
    } finally {
      $client.Dispose()
    }
  }

  Invoke-Button (Find-Button 'Continue' 45)
  $start = Find-Button 'Start installation' 20
  $deploy = [System.Diagnostics.Stopwatch]::StartNew()
  Invoke-Button $start
  $enter = Find-Button 'Configure model' 600
  $ready = (Get-Content $checkpoint -Raw | ConvertFrom-Json).progress.state
  if ($ready -ne 'ready') { throw "The native checkpoint was '$ready' after device setup." }
  "First-run devices ready in $($deploy.ElapsedMilliseconds) ms"
  Wait-WorkbenchHealth
  $setupWindow = (Get-Process -Id $env:AIMANITY_QA_PID).MainWindowHandle
  $navigation = & curl.exe --noproxy '*' --silent --show-error --max-time 4 --head 'http://127.0.0.1:17860/phone?language=en' |
    Where-Object { $_ -match '^(HTTP/|location:)' }
  Write-Host "First-open redirect: $($navigation -join ' ')"
  Invoke-Button $enter

  $owned = [System.Windows.Automation.PropertyCondition]::new($element::ProcessIdProperty, [int]$env:AIMANITY_QA_PID)
  $document = [System.Windows.Automation.PropertyCondition]::new($element::ControlTypeProperty, [System.Windows.Automation.ControlType]::Document)
  $desktop = $element::RootElement
  $deadline = [DateTime]::UtcNow.AddSeconds(30)
  do {
    $windows = $desktop.FindAll([System.Windows.Automation.TreeScope]::Children, $owned)
    foreach ($window in $windows) {
      if ($window.Current.NativeWindowHandle -eq $setupWindow.ToInt32() -or $window.Current.IsOffscreen) { continue }
      $content = $window.FindFirst($descendants, $document)
      if ($content -and $content.Current.Name -like '*AImanity*') {
        "Installed workbench HWND $($window.Current.NativeWindowHandle); window='$($window.Current.Name)'; document='$($content.Current.Name)'"
        exit 0
      }
    }
    Start-Sleep -Milliseconds 450
  } while ([DateTime]::UtcNow -lt $deadline)
  $diagnostics = [System.Windows.Automation.PropertyCondition]::new($element::IsOffscreenProperty, $false)
  foreach ($window in $windows) {
    Write-Host "AImanity window $($window.Current.NativeWindowHandle); title='$($window.Current.Name)'; class='$($window.Current.ClassName)'"
    $window.FindAll($descendants, $diagnostics) |
      Where-Object { $_.Current.Name } | Select-Object -First 35 |
      ForEach-Object { Write-Host "  $($_.Current.ControlType.ProgrammaticName) '$($_.Current.Name)' ($($_.Current.AutomationId))" }
  }
  throw 'The installed workbench window did not appear.'
`;

try {
  const script = Buffer.from(automation, "utf16le").toString("base64");
  const desktopSetup = Bun.spawn(["powershell.exe", "-NoProfile", "-NonInteractive", "-EncodedCommand", script], {
    env: { ...env, AIMANITY_QA_PID: String(app.pid) }, stdout: "inherit", stderr: "inherit",
  });
  const desktopSetupExit = await desktopSetup.exited;
  if (desktopSetupExit !== 0) {
    ok(existsSync(checkpoint), "Windows setup stopped before creating its checkpoint");
    const state = JSON.parse(readFileSync(checkpoint, "utf8")).progress.state;
    equal(state, "ready", "Windows setup must reach actual dual-device readiness before checking the workbench");
    console.error("Workbench UI failed. Checking the independently served workbench and real devices.");
  }

  const protocol = "application/x-protobuf";
  const login = await fetch(`${origin}/api/auth/enter`, { method: "POST",
    headers: { "content-type": protocol, origin }, body: new Uint8Array(), signal: AbortSignal.timeout(10000) });
  equal(login.status, 200, "workbench sign-in");
  const setCookie = login.headers.get("set-cookie");
  ok(setCookie, "workbench issued a session");
  const cookie = setCookie.split(";", 1)[0]!;
  const registry = await fetch(`${origin}/api/registry/state`, { method: "POST",
    headers: { cookie, "content-type": protocol, origin },
    body: toBinary(RegistryRequestSchema, create(RegistryRequestSchema)), signal: AbortSignal.timeout(10000) });
  equal(registry.status, 200, "installed service registry");
  const devices = fromBinary(RegistryStateSchema, new Uint8Array(await registry.arrayBuffer())).devices;
  const desktop = devices.find((device) => device.kind === "desktop" && device.runtimeState === "managed");
  const phone = devices.find((device) => device.kind === "phone" && device.runtimeState === "managed");
  ok(desktop, "installed desktop is registered");
  ok(phone, "installed Android is registered");
  async function verifyFrame(endpoint: string, device: string) {
    const screen = await fetch(`${origin}${endpoint}`, { headers: { cookie }, signal: AbortSignal.timeout(12000) });
    equal(screen.status, 200, `${device} media HTTP status`);
    match(screen.headers.get("content-type") ?? "", /^multipart\/x-mixed-replace/i);
    const reader = screen.body!.getReader();
    let frames = Buffer.alloc(0);
    const start = Buffer.from([0xff, 0xd8, 0xff]), end = Buffer.from([0xff, 0xd9]);
    try {
      while (frames.length < 2_000_000) {
        const { value, done } = await reader.read();
        if (done) break;
        frames = Buffer.concat([frames, Buffer.from(value)]);
        const offset = frames.indexOf(start);
        if (offset >= 0 && frames.indexOf(end, offset + 3) > offset) {
          console.log(`${device} sent a complete JPEG through the workbench`);
          return true;
        }
      }
    } finally { await reader.cancel(); }
    return false;
  }
  async function verifyAndroidFrame(endpoint: string) {
    const deadline = performance.now() + 12_000;
    for (let attempt = 1; ; attempt++) {
      if (await verifyFrame(endpoint, "Android")) {
        console.log(JSON.stringify({ androidJpegAttempts: attempt }));
        return;
      }
      if (performance.now() >= deadline) throw Error("Android did not send a complete JPEG");
      console.log(`Android screen subscription ${attempt} closed during startup without a JPEG`);
      await Bun.sleep(750);
    }
  }
  await Promise.all([
    verifyFrame(`/api/desktop/media?device_id=${encodeURIComponent(desktop.id)}`, "Linux").then(passed => ok(passed, "Linux JPEG stream")),
    verifyAndroidFrame(`/api/phone/media/screen?device=${encodeURIComponent(phone.id)}`),
  ]);
  const scope = { phoneId: phone.id, ownerId: "local-user", controller: "manual" };
  async function phoneRequest(operation: string, body: Uint8Array) {
    const response = await fetch(`${origin}/api/phone/${operation}`, { method: "POST",
      headers: { cookie, origin, "content-type": protocol }, body: new Uint8Array(body), signal: AbortSignal.timeout(16000) });
    equal(response.status, 200, `Android ${operation} HTTP status`);
    return new Uint8Array(await response.arrayBuffer());
  }
  const control = fromBinary(ControlStateSchema, await phoneRequest("control",
    toBinary(TakeControlRequestSchema, create(TakeControlRequestSchema, scope))));
  async function phoneAction(action: MessageInitShape<typeof DeviceActionRequestSchema>["action"]) {
    const actionId = crypto.randomUUID();
    const request = create(DeviceActionRequestSchema, { ...scope, actionId, runId: "manual", controlEpoch: control.controlEpoch, action });
    const result = fromBinary(DeviceActionResponseSchema, await phoneRequest("action", toBinary(DeviceActionRequestSchema, request)));
    equal(result.actionId, actionId);
    equal(result.success, true, `Android ${action?.case} did not execute: ${result.output}`);
    return result;
  }

  const components = join(home, "components", "android");
  const [identity] = readdirSync(components);
  ok(identity, "installed Android capsule");
  const adb = join(components, identity, "sdk", "platform-tools", "adb.exe");
  function android(...args: string[]) {
    const result = Bun.spawnSync([adb, "-P", "15037", "-s", "127.0.0.1:5565", ...args], { env });
    equal(result.exitCode, 0, "installed Android ADB command");
    return result.stdout;
  }
  equal(android("shell", "id", "-u").toString().trim(), "0");
  const frame = android("exec-out", "screencap", "-p");
  equal(frame.subarray(0, 8).toString("hex"), "89504e470d0a1a0a");
  deepEqual([frame.readUInt32BE(16), frame.readUInt32BE(20)], [720, 1440]);
  const policy = android("shell", "dumpsys", "window", "policy").toString().split("\n")
    .filter(line => /keyguard|showing|interactive/i.test(line)).slice(0, 14).join("\n");
  const beforeFocus = android("shell", "dumpsys", "window", "displays").toString().split("\n")
    .filter(line => /mCurrentFocus|mFocusedApp/.test(line)).join("\n");
  console.log(`Android pristine window state:\n${beforeFocus}\n${policy}`);
  match(android("shell", "locksettings", "set-disabled", "--user", "0", "true").toString(), /Lock screen disabled set to true/);
  equal(android("shell", "locksettings", "get-disabled", "--user", "0").toString().trim(), "true");
  android("shell", "input", "keyevent", "KEYCODE_SLEEP");
  await Bun.sleep(350);
  android("shell", "input keyevent KEYCODE_WAKEUP; svc power stayon true; cmd statusbar collapse; wm dismiss-keyguard; input keyevent KEYCODE_HOME");
  const search = "com.android.settings.intelligence";
  const opened = android("shell", "am", "start", "-W", "-a", "android.settings.APP_SEARCH_SETTINGS", "-p", search).toString();
  match(opened, /Status: ok\b/);
  equal(android("shell", "settings", "get", "secure", "default_input_method").toString().trim(), "dev.aiphone.input/.UnicodeIme");
  const uiFile = "/sdcard/aimanity-install.xml";
  async function androidEditor(predicate: (xml: string) => boolean) {
    const deadline = Date.now() + 15000;
    let last = "";
    do {
      last = android("shell", `rm -f ${uiFile}; timeout 7 uiautomator dump ${uiFile} 2>&1; [ ! -s ${uiFile} ] || cat ${uiFile}`).toString();
      if (predicate(last)) return last;
      await Bun.sleep(250);
    } while (Date.now() < deadline);
    const focus = android("shell", "dumpsys", "window", "displays").toString().split("\n")
      .filter(line => /mCurrentFocus|mFocusedApp/.test(line)).join("\n");
    throw Error(`Android Settings editor not found: ${focus}\n${last.slice(0, 450)}`);
  }
  const editable = /<node[^>]*resource-id="android:id\/search_src_text"[^>]*bounds="\[(\d+),(\d+)\]\[(\d+),(\d+)\]"/;
  const clear = /<node[^>]*resource-id="android:id\/search_close_btn"[^>]*bounds="\[(\d+),(\d+)\]\[(\d+),(\d+)\]"/;
  async function tap(field: RegExpMatchArray | null) {
    ok(field, "visible Android control coordinates");
    await phoneAction({case: "tap", value: { x: (Number(field[1]) + Number(field[3])) >> 1,
      y: (Number(field[2]) + Number(field[4])) >> 1 }});
  }
  const hierarchy = await androidEditor(xml => editable.test(xml));
  await tap(hierarchy.match(editable));
  await androidEditor(xml => editable.test(xml) && editable.exec(xml)?.[0].includes('focused="true"') === true);
  const sample = "Aluminum 中文";
  const input = await phoneAction({case: "input", value: {text: sample}});
  ok(input.output.includes(`committed:${input.actionId}`), "installed Unicode IME returned the action receipt");
  const entered = await androidEditor(xml => editable.exec(xml)?.[0].includes(`text="${sample}"`) === true && clear.test(xml));
  await tap(entered.match(clear));
  await androidEditor(xml => editable.test(xml) && !editable.exec(xml)?.[0].includes(sample) && !clear.test(xml));
  console.log(JSON.stringify({ androidRoot: true, androidFrame: [720, 1440], androidWorkbenchTouch: true, androidWorkbenchChinese: true, linuxJpeg: true }));

  if (process.env.AIMANITY_QA_ANDROID_AUDIO === "1") {
    const music = android("shell", "pm", "list", "packages", "--user", "0", "com.android.music").toString().trim();
    if (music !== "package:com.android.music") {
      match(android("shell", "cmd", "package", "install-existing", "--user", "0", "com.android.music").toString(), /installed for user: 0/);
    }
    const audio = await fetch(`${origin}/api/phone/media/audio?device=${encodeURIComponent(phone.id)}`, {
      headers: { cookie }, signal: AbortSignal.timeout(16000),
    });
    equal(audio.status, 200, "installed Android audio");
    match(audio.headers.get("content-type") ?? "", /^audio\/L16;rate=48000;channels=2$/i);
    const pcm = audio.body!.getReader();
    let min = 0, max = 0, samples = 0;
    let partial: Buffer<ArrayBufferLike> = Buffer.alloc(0);
    try {
      const playback = android("shell", "am", "start", "-W", "-a", "android.intent.action.VIEW", "-d",
        "file:///product/media/audio/alarms/Neon.ogg", "-t", "audio/ogg", "-n", "com.android.music/.AudioPreview");
      match(playback.toString(), /Status: ok\b/);
      while (min === 0 || max === 0) {
        const { value, done } = await pcm.read();
        if (done) break;
        const bytes = Buffer.concat([partial, Buffer.from(value)]);
        const complete = bytes.length & ~1;
        for (let index = 0; index < complete; index += 2) {
          const sample = bytes.readInt16LE(index);
          min = Math.min(min, sample);
          max = Math.max(max, sample);
          samples++;
        }
        partial = bytes.subarray(complete);
      }
    } finally {
      await pcm.cancel();
      android("shell", "am", "force-stop", "com.android.music");
      if (music !== "package:com.android.music") android("shell", "pm", "uninstall", "-k", "--user", "0", "com.android.music");
    }
    ok(min < 0 && max > 0, "installed Android audio emitted nonzero stereo PCM");
    console.log(JSON.stringify({ androidAudio: true, rate: 48000, channels: 2, samples, min, max }));
  }
  equal(desktopSetupExit, 0, "real Windows accessibility first-run and workbench document");
  if (process.env.AIMANITY_QA_BERYLLIUM_FRESH === "1") {
    const packages = android("shell", "pm", "list", "packages", "--user", "0").toString().split(/\r?\n/);
    for (const installed of ["info.plateaukao.einkbro", "com.android.settings", "com.android.documentsui", "dev.aiphone.input"]) {
      ok(packages.includes(`package:${installed}`), `${installed} is available`);
    }
    for (const retired of ["com.github.metacubex.clash.meta", "com.android.dialer", "com.android.messaging", "com.android.camera2", "com.android.deskclock", "com.android.calendar"]) {
      ok(!packages.includes(`package:${retired}`), `${retired} is absent`);
    }
    const launcher = android("shell", "cmd", "package", "resolve-activity", "--brief", "--user", "0", "-a", "android.intent.action.MAIN", "-c", "android.intent.category.HOME").toString();
    match(launcher, /dev\.aiphone\.home\/\.HomeActivity/);
    console.log("Beryllium phone default packages and paged Home verified");
  }

  if (process.env.AIMANITY_QA_BERYLLIUM_UPGRADE === "1") {
    const expectedName = `update-ownership-${crypto.randomUUID()}.txt`;
    const guestFolder = "/home/ai/Files";
    const updateStart = performance.now();
    const diskIdentity = join(home, "user", "qemu", "disk", "base.sha256");
    const legacyIdentity = "348baa7207524397b74de603f08357b6d3b73947514eb9f662c6eb4e52a365a5";
    equal(readFileSync(diskIdentity, "utf8").trim(), legacyIdentity);
    const staged = new FormData();
    staged.append("location", new Blob([toBinary(FileLocationSchema, create(FileLocationSchema, {path: "/"}))], {type: protocol}));
    staged.append("file", new File([expectedName], expectedName, {type: "text/plain"}));
    const uploaded = await fetch(`${origin}/api/files/upload`, {method: "POST", headers: {cookie, origin}, body: staged, signal: AbortSignal.timeout(10000)});
    equal(uploaded.status, 200, "saved prior workspace file");
    const location = fromBinary(FileLocationSchema, new Uint8Array(await uploaded.arrayBuffer()));
    const uploadedName = location.path.split("/").at(-1);
    const copy = await fetch(`${origin}/api/files/manage`, {method: "POST", headers: {cookie, origin, "content-type": protocol},
      body: toBinary(FileManageRequestSchema, create(FileManageRequestSchema, {operation: FileOperation.COPY, sources: [location], target: {path: guestFolder, deviceId: desktop.id}})),
      signal: AbortSignal.timeout(15000)});
    equal(copy.status, 200);
    equal(fromBinary(FileManageResultSchema, new Uint8Array(await copy.arrayBuffer())).results[0]?.error, "", "wrote prior Linux overlay file through workbench");
    const phonePath = `/sdcard/Download/${expectedName}`;
    android("shell", `printf %s '${expectedName}' > ${phonePath} && sync`);
    equal(android("shell", "cat", phonePath).toString(), expectedName, "prior virtual phone persisted exact content before updating");
    const updatePattern = "AImanity-0.0.2-windows-x64-setup.exe";
    const updateFolder = process.env.AIMANITY_QA_UPDATE_CACHE!;
    mkdirSync(updateFolder, {recursive: true});
    const helperFile = join(updateFolder, "ai-phone-update-helper.exe");
    copyFileSync(program, helperFile);
    copyFileSync(join(root, updatePattern), join(updateFolder, updatePattern));
    const helper = Bun.spawn([helperFile, "--install-client-update", String(app.pid), "0.0.2"], {
      env, stdin: "ignore", stdout: "pipe", stderr: "inherit",
    });
    const ready = helper.stdout.getReader();
    equal((await ready.read()).value?.[0], 1, "the installed client's signed-updater helper acquired the official installation and EXE");
    ready.releaseLock();
    const close = Bun.spawnSync(["powershell.exe", "-NoProfile", "-NonInteractive", "-Command", `(Get-Process -Id ${app.pid}).CloseMainWindow()`], {env});
    equal(close.stdout.toString().trim(), "True", "close Aluminum so the updater installs and relaunches");
    equal(await helper.exited, 0, "Aluminum's exact update helper installer and relaunch exit code");
    equal(readFileSync(diskIdentity, "utf8").trim(), legacyIdentity, "previous Linux QCOW backing identity survives");
    const release = JSON.parse(readFileSync(join(install, "host", "qemu", "manifest.json"), "utf8"));
    equal(release.source_commit, process.env.BERYLLIUM_SOURCE, "new source release layer");
    const deadline = performance.now() + 600_000;
    let upgradedCookie = "";
    do {
      const progress = JSON.parse(readFileSync(checkpoint, "utf8")).progress;
      if (progress.state === "failed") throw Error(`Updated device setup failed: ${progress.error}`);
      if (progress.state === "ready") {
        const response = await fetch(`${origin}/api/auth/enter`, {method: "POST", headers: {origin, "content-type": protocol}, body: new Uint8Array(), signal: AbortSignal.timeout(2000)}).catch(() => undefined);
        if (response?.ok) upgradedCookie = response.headers.get("set-cookie")?.split(";", 1)[0] ?? "";
      }
      if (upgradedCookie) break;
      await Bun.sleep(750);
    } while (performance.now() < deadline);
    ok(upgradedCookie, "native updater reopened the newly deployed workbench");
    async function checkRetained(id: string, path: string) {
      const listing = await fetch(`${origin}/api/files/list`, {method: "POST", headers: {cookie: upgradedCookie, origin, "content-type": protocol},
        body: toBinary(FileLocationSchema, create(FileLocationSchema, {deviceId:id,path})), signal: AbortSignal.timeout(20000)});
      equal(listing.status, 200, `new workbench lists prior ${path}`);
      const {entries} = fromBinary(FileListingSchema, new Uint8Array(await listing.arrayBuffer()));
      ok(entries.some(entry => entry.name === uploadedName && entry.sizeBytes === BigInt(expectedName.length)), `prior ${path} retained exact file length and uploaded name`);
    }
    await checkRetained("", "/");
    await checkRetained(desktop.id, guestFolder);
    equal(android("shell", "cat", phonePath).toString(), expectedName, "prior virtual phone retained exact content");
    console.log(JSON.stringify({upgrade: "0.0.1 → 0.0.2; prior overlay data preserved", release: release.release_sha256,
      workspace: true, linux: true, android: true, ms: Math.round(performance.now() - updateStart)}));
  }

} catch (error) {
  const directory = join(home, "components", "android");
  if (existsSync(directory)) {
    const [identity] = readdirSync(directory);
    const adb = join(directory, identity!, "sdk", "platform-tools", "adb.exe");
    const guest = (command: string) => {
      const output = Bun.spawnSync([adb, "-P", "15037", "-s", "127.0.0.1:5565", "shell", command], {env, timeout: 12_000});
      console.log(`Windows setup diagnostic ${command}: code=${output.exitCode} ${output.stdout.toString().trim()} ${output.stderr.toString().trim()}`);
    };
    guest("getprop sys.boot_completed; id -u; pm list packages --show-versioncode --user 0; cmd package resolve-activity --brief --user 0 -a android.intent.action.MAIN -c android.intent.category.HOME; settings get secure default_input_method; ls -l /data/local/tmp/ai-phone-input-notification-probe.jar");
    guest("cmd role get-role-holders --user 0 android.app.role.BROWSER");
    guest("logcat -d -t 600 -s PackageManager PackageInstaller PermissionController RoleControllerService AndroidRuntime | tail -60");
  }
  for (const log of logPaths) {
    const file = join(home, log);
    if (existsSync(file)) {
      const output = readFileSync(file, "utf8");
      const milestones = output.split("\n").filter(line => /adbd|boot_completed|keyguard|SurfaceFlinger|panic|WHPX|SystemServer|FATAL EXCEPTION/i.test(line)).slice(-32);
      console.error(log, milestones.join("\n"), output.slice(-3600));
    }
  }
  throw error;
} finally {
  clearInterval(watchWeb);
  clearInterval(watchAndroid);
  if (app.exitCode === null) app.kill();
}
