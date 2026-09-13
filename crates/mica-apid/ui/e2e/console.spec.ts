import { expect, test, type Page } from '@playwright/test'

test.beforeEach(async ({ page }) => {
  await mockDeviceApi(page)
})

test('navigates the complete desktop console', async ({ page, isMobile }) => {
  test.skip(isMobile, 'desktop navigation contract')
  await page.goto('./')

  await expect(page.getByRole('heading', { name: 'Overview' })).toBeVisible()
  await expect(page.getByRole('navigation', { name: 'Primary navigation' })).toBeVisible()
  await page.getByRole('link', { name: 'Applications' }).click()
  await expect(page.getByRole('heading', { name: 'Applications' })).toBeVisible()
  await expect(page.getByText('Simulation', { exact: true })).toBeVisible()
  await page.getByRole('tab', { name: 'Catalog' }).click()
  const mqtt = page.locator('[data-slot=card]').filter({ hasText: 'MQTT Bridge' })
  await mqtt.getByRole('button', { name: 'Install' }).click()
  await page.getByRole('button', { name: 'Next' }).click()
  await page.getByRole('button', { name: 'Next' }).click()
  await page.getByRole('button', { name: 'Install 2.4.0' }).click()
  await expect(page.getByText('MQTT Bridge is running')).toBeVisible()
  await page.getByRole('button', { name: 'Open application' }).click()
  await page.getByRole('tab', { name: 'Installed' }).click()
  await expect(page.getByRole('button', { name: 'MQTT Bridge', exact: true })).toBeVisible()
})

test('routes the reported update from the overview attention list', async ({ page, isMobile }) => {
  test.skip(isMobile, 'desktop navigation contract')
  await page.goto('./')

  await expect(page.getByText('System update 2026.09.0 is ready to install')).toBeVisible()
  await page.getByRole('link', { name: 'Open Update' }).click()
  await expect(page.getByRole('heading', { name: 'System' })).toBeVisible()
  await expect(page.getByRole('tab', { name: 'Update & recovery', selected: true })).toBeVisible()
})

test('writes real time settings while the simulation boundary stays named', async ({ page, isMobile }) => {
  test.skip(isMobile, 'desktop system workflow')
  await page.goto('./system')
  await page.getByRole('tab', { name: 'Time' }).click()

  await expect(page.getByRole('heading', { name: 'System' })).toBeVisible()
  await expect(page.getByRole('heading', { name: 'NTP servers' })).toBeVisible()
  await expect(page.getByText('the kernel reports a bounded clock error')).toBeVisible()
  await page.getByRole('textbox', { name: /^Timezone/ }).fill('Europe/Berlin')
  await page.getByRole('button', { name: 'Save timezone' }).click()
  await expect(page.getByText('Change applied.')).toBeVisible()
  await expect(page.getByText('Simulation', { exact: true })).toBeVisible()
})

test('reads the observed network beside the desired configuration', async ({ page, isMobile }) => {
  test.skip(isMobile, 'desktop network workflow')
  await page.goto('./network')

  await expect(page.getByText(/Observed state only/)).toBeVisible()
  await expect(page.getByText('192.168.1.24/24 \u00b7 DHCPv4')).toBeVisible()
  await expect(page.getByText('Cellular')).toBeVisible()
  await expect(page.getByText('unsupported').first()).toBeVisible()
})

test('restores Chinese and dark appearance preferences', async ({ page, isMobile }) => {
  test.skip(isMobile, 'desktop appearance contract')
  await page.addInitScript(() => {
    localStorage.setItem('mica.ui.locale', 'zh-CN')
    localStorage.setItem('mica.ui.theme', 'dark')
  })
  await page.goto('./')

  await expect(page.getByRole('heading', { name: '概览' })).toBeVisible()
  await expect(page.locator('html')).toHaveClass(/dark/)
  await expect(page.locator('html')).toHaveAttribute('lang', 'zh-CN')
})

test('applies a typed interface edit through the review dialog', async ({ page, isMobile }) => {
  test.skip(isMobile, 'desktop network editor contract')
  await page.goto('./network')
  await page.getByRole('link', { name: 'eth0' }).click()

  await expect(page.getByRole('heading', { name: 'eth0' })).toBeVisible()
  // The mode control is the registry's toggle group: single-select buttons
  // carrying aria-pressed, where the page previously hand-rolled a radiogroup.
  await page.getByRole('button', { name: 'Static', exact: true }).click()
  await page.getByRole('textbox', { name: 'Address / prefix' }).fill('192.168.1.24/24')
  await page.getByRole('button', { name: 'Review and save' }).click()

  await expect(page.getByRole('dialog')).toContainText('this browser did not arrive on eth0')
  const request = page.waitForRequest((candidate) => candidate.method() === 'PUT')
  await page.getByRole('button', { name: 'Apply', exact: true }).click()
  const interfaceRequest = await request
  expect(new URL(interfaceRequest.url()).pathname).toBe('/api/v1/network/eth0')
  expect(interfaceRequest.postDataJSON()).toMatchObject({ dhcp: false, static: { address: '192.168.1.24/24' } })
  await expect(page.getByText('Applying the change')).toBeVisible()
})

test('shows the new WireGuard public key after rotation', async ({ page }, testInfo) => {
  test.skip(testInfo.project.name !== 'chromium', 'one browser covers the API response contract')
  await page.goto('./network')
  await page.getByRole('tab', { name: 'WireGuard' }).click()
  await page.getByRole('button', { name: 'Rotate key' }).click()
  await page.getByRole('button', { name: 'Rotate key' }).last().click()

  await expect(page.getByText('rotated-e2e-public-key=')).toBeVisible()
})

test('uses the mobile navigation drawer', async ({ page, isMobile }) => {
  test.skip(!isMobile, 'mobile navigation contract')
  await page.goto('./')

  await page.getByRole('button', { name: 'Open navigation' }).click()
  await expect(page.getByRole('navigation', { name: 'Primary navigation' })).toBeVisible()
  await page.getByRole('link', { name: 'Network' }).click()
  await expect(page.getByRole('heading', { name: 'Network' })).toBeVisible()
})

test('labels every route that contains simulated behavior', async ({ page }, testInfo) => {
  test.skip(testInfo.project.name !== 'chromium', 'one browser covers the shared route boundary')
  for (const path of ['./services', './applications', './system']) {
    await page.goto(path)
    await expect(page.getByText('Simulation', { exact: true })).toBeVisible()
    await expect(page.getByText(/does not change this device/)).toBeVisible()
    if (path === './services') {
      await page.getByRole('link', { name: 'Web terminal' }).click()
      await page.getByRole('switch', { name: 'Web terminal' }).click()
      await page.getByRole('button', { name: 'Open terminal' }).click()
      const terminal = page.getByRole('dialog', { name: 'Web terminal' })
      await expect(terminal).toBeVisible()
      await expect(terminal).toContainText('accepts no input')
      await terminal.getByRole('button', { name: 'End session' }).click()
    }
  }
})

test('keeps a tall dialog inside a short appliance screen', async ({ page }, testInfo) => {
  test.skip(testInfo.project.name !== 'chromium', 'one browser covers the geometry')
  // A 1024x600 panel is a normal appliance display. The dialog used to be
  // centred with no height bound and no internal scroll, so at 520px its title
  // sat at -37px and both footer buttons were below the fold.
  await page.setViewportSize({ width: 1024, height: 520 })
  await page.goto('./network')
  await page.getByRole('button', { name: 'Add interface' }).click()
  await page.getByRole('switch', { name: 'Use DHCP' }).click()

  const dialog = page.locator('[data-slot=dialog-content]')
  const box = (await dialog.boundingBox())!
  expect(box.y).toBeGreaterThanOrEqual(0)
  expect(box.y + box.height).toBeLessThanOrEqual(520)
  await expect(page.getByRole('heading', { name: 'Add network interface' })).toBeVisible()
  await expect(page.getByRole('button', { name: 'Add', exact: true })).toBeVisible()
  await page.getByRole('button', { name: 'Cancel' }).click()
  await expect(dialog).toHaveCount(0)
})

test('opens the language picker without a second layer behind it', async ({ page }, testInfo) => {
  test.skip(testInfo.project.name !== 'chromium', 'one browser covers the overlay contract')
  // The picker used to be a dialog rendered inside the header menu, which left
  // the menu open and painted above its own backdrop.
  await page.goto('./')
  await page.getByRole('combobox', { name: 'Language' }).click()

  await expect(page.getByRole('option', { name: /简体中文/ })).toBeVisible()
  await expect(page.getByRole('dialog')).toHaveCount(0)
  await expect(page.locator('[data-slot=dropdown-menu-popup]')).toHaveCount(0)
})

test('confirms a destructive action by closing it and saying what happened', async ({ page }, testInfo) => {
  test.skip(testInfo.project.name !== 'chromium', 'one browser covers the confirmation contract')
  // Ten of the eleven confirmations left the modal open with nothing said, so
  // a completed action and a lost click looked identical.
  await page.goto('./system')
  await page.getByRole('button', { name: 'Reboot' }).click()
  const dialog = page.getByRole('alertdialog')
  await expect(dialog).toBeVisible()
  await dialog.getByRole('button', { name: 'Reboot' }).click()

  await expect(dialog).toHaveCount(0)
  await expect(page.getByText('Power action accepted.').first()).toBeVisible()
})

test('reports a saved policy, which used to change nothing on the page', async ({ page }, testInfo) => {
  test.skip(testInfo.project.name !== 'chromium', 'one browser covers the feedback contract')
  await page.goto('./system')
  await page.getByRole('tab', { name: /Update/ }).click()
  await page.getByRole('button', { name: 'Save policy' }).click()

  await expect(page.getByText('Update policy saved.').first()).toBeVisible()
})

test('matches the approved responsive visual baseline', async ({ page }, testInfo) => {
  if (testInfo.project.name === 'tablet') {
    await page.addInitScript(() => localStorage.setItem('mica.ui.theme', 'dark'))
    await page.goto('./applications')
    await page.getByRole('tab', { name: 'Catalog' }).click()
    await expect(page.locator('html')).toHaveClass(/dark/)
    await expect(page).toHaveScreenshot('applications-tablet-dark.png', { animations: 'disabled', fullPage: true })
    return
  }

  await page.goto('./')
  if (testInfo.project.name === 'mobile') {
    await page.getByRole('button', { name: 'Open navigation' }).click()
    await expect(page).toHaveScreenshot('overview-mobile-drawer-light.png', { animations: 'disabled', fullPage: true })
    return
  }
  await expect(page).toHaveScreenshot('overview-desktop-light.png', { animations: 'disabled', fullPage: true })
})

async function mockDeviceApi(page: Page) {
  await page.route('**/api/v1/**', async (route) => {
    const request = route.request()
    const path = new URL(request.url()).pathname
    const body = payload(path, request.method())
    await route.fulfill({
      status: body === undefined ? 204 : 200,
      contentType: body === undefined ? undefined : 'application/json',
      body: body === undefined ? undefined : JSON.stringify(body),
    })
  })
}

const FIVE_MINUTES_AGO = () => new Date(Date.now() - 5 * 60_000).toISOString()

function payload(path: string, method: string): unknown {
  if (method !== 'GET') {
    if (path === '/api/v1/ui/active') return { mode: 'builtIn' }
    if (path === '/api/v1/actions/wireguard/wg0/rotate-key') return { publicKey: 'rotated-e2e-public-key=' }
    return { taskId: 'task-e2e' }
  }
  if (path === '/api/v1/session') return { state: 'authenticated', csrfToken: 'e2e-csrf' }
  if (path === '/api/v1/health') return { apid: 'ok', micad: 'ok', checkedAt: 183900 }
  if (path === '/api/v1/meta') return { api: 'v1', settingsSchemaVersion: 4, daemon: 'micad 0.1.0' }
  if (path === '/api/v1/system/info') return {
    machineId: { available: true, id: '4f2e9c1a7b3d4e5f' },
    board: { available: true, model: 'mica-cm4 rev 2', source: 'device-tree' },
    kernel: { available: true, release: '6.6.52-mica', version: '#1 SMP' },
    release: { available: true, name: 'mica', versionId: '2026.08.2', imageVersion: '2026.08.2', prettyName: 'mica 2026.08.2' },
    system: { available: true, version: '2026.08.2', package: 'mica-system', buildDate: '2026-08-19' },
    daemon: { available: true, name: 'micad', version: '0.1.0', commit: 'a3f9c1e' },
    packages: { available: true, count: 214, micaCount: 12, entries: [] },
    slot: { available: true, booted: 'A', bootname: 'rootfs.0', bootStatus: 'good', primary: true },
    uptime: { available: true, seconds: 1231932 },
  }
  if (path === '/api/v1/settings/hostname') return 'mica-cm4'
  if (path === '/api/v1/network') return {
    configured: { eth0: { dhcp: true }, wg0: { kind: 'wireguard', dhcp: false, static: { address: '10.10.0.2/24', dns: [] }, wireguard: { listenPort: 51820, peers: [] } } },
    configuredCount: 2,
    observed: { available: true, interfaceCount: 2, interfaces: [{ index: 2, name: 'eth0', kind: 'ether', operationalState: 'routable', addresses: ['192.168.1.24/24'] }, { index: 4, name: 'wg0', kind: 'wireguard', operationalState: 'routable', addresses: ['10.10.0.2/24'] }] },
  }
  if (path === '/api/v1/tasks') return [{ id: 'task-1', operation: 'set', dotPath: 'hostname', source: 'api', status: 'finished', outcome: 'succeeded', enqueuedAt: FIVE_MINUTES_AGO(), foldedCount: 0 }]
  if (path === '/api/v1/tasks/task-e2e') return { id: 'task-e2e', operation: 'set', dotPath: 'settings', source: 'api', status: 'finished', outcome: 'succeeded', enqueuedAt: FIVE_MINUTES_AGO(), foldedCount: 0 }
  if (path === '/api/v1/ui') return { mode: 'builtIn' }
  if (path === '/api/v1/settings/container.enabled' || path === '/api/v1/settings/mqtt.enabled' || path === '/api/v1/settings/access.ssh.enabled') return true
  if (path === '/api/v1/state/container' || path === '/api/v1/state/mqtt') return { state: 'running' }
  if (path === '/api/v1/tokens') return []
  if (path === '/api/v1/ssh/authorized-keys') return { keys: [], notice: 'Every authorized key grants root on this appliance.' }
  if (path === '/api/v1/settings/wifi.client') return { enabled: true, interface: 'wlan0', networks: [] }
  if (path === '/api/v1/wifi/client/networks' || path.endsWith('/peers')) return []
  if (path === '/api/v1/update') return { lifecycle: { state: 'ready', available: { name: 'mica', version: '2026.09.0', channel: 'stable' }, reboot_gate: { safe: true, reasons: [] }, client: { available: true } }, booted_slot: 'rootfs.0' }
  if (path === '/api/v1/settings/time.ntp.servers') return ['0.pool.ntp.org']
  if (path === '/api/v1/settings/time.timezone') return 'Etc/UTC'
  if (path === '/api/v1/time/status') return {
    status: 'synchronized',
    synchronized: true,
    server: { name: 'time.cloudflare.com', address: '162.159.200.1' },
    sample: { leap: 0, stratum: 3, spike: false, offsetSeconds: 0.0024, packetCount: 8, correction: 'slew' },
  }
  if (path === '/api/v1/network/status') return {
    interfaces: { available: true, count: 1, entries: [{
      name: 'eth0',
      link: { operationalState: 'routable', carrierState: 'carrier', carrier: true },
      addresses: [{ family: 'inet', address: '192.168.1.24', prefixLength: 24, configSource: 'DHCPv4' }],
      dhcp: { available: true, state: 'bound', lease: { server: '192.168.1.1' } },
      dns: ['192.168.1.1'],
    }] },
    defaultRoutes: { available: true, count: 1, entries: [{ family: 'inet', gateway: '192.168.1.1', interface: 'eth0', metric: 100 }] },
    dns: { available: true, linkServers: ['192.168.1.1'], resolverServers: ['127.0.0.53'], probe: { name: 'deb.debian.org', reachable: true, result: 'resolved' } },
    wifi: { available: true, associations: [] },
    capabilities: { wifi: { supported: false, interfaces: [] }, bluetooth: { supported: false, adapters: [] }, cellular: { supported: false, interfaces: [] } },
  }
  if (path === '/api/v1/ui/bundles') return { bundles: [], retentionLimit: 32 }
  return {}
}
