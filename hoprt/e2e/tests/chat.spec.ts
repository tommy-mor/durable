import { test, expect } from '@playwright/test';
import { openTab, startHopd, stopHopd } from '../helpers';

test('chat: lobby broadcasts; a second room is isolated until you open it', async ({ browser }) => {
  const hopd = await startHopd('hoprt/hop/chat.hop', 19130, 19131);
  try {
    const ctxA = await browser.newContext();
    const ctxB = await browser.newContext();
    const a = await ctxA.newPage();
    const b = await ctxB.newPage();
    await openTab(a, hopd.url);
    await openTab(b, hopd.url);

    await expect(a.locator('#room-name')).toContainText('room lobby');
    await a.locator('#draft').fill('hello tape');
    await a.locator('#send').click();
    await expect(a.locator('#messages li')).toContainText('hello tape');
    await expect(b.locator('#messages li')).toContainText('hello tape');

    await a.locator('#new-room').fill('random');
    await a.locator('#open-room').click();
    await expect(a.locator('#room-name')).toContainText('room random');
    await a.locator('#draft').fill('side channel');
    await a.locator('#send').click();
    await expect(a.locator('#messages li')).toContainText('side channel');
    // B is still in lobby and must not see the side-channel line
    await expect(b.locator('#room-name')).toContainText('room lobby');
    await expect(b.locator('#messages li')).toContainText('hello tape');
    await expect(b.locator('#messages li')).not.toContainText('side channel');

    await b.locator('#room-random').click();
    await expect(b.locator('#room-name')).toContainText('room random');
    await expect(b.locator('#messages li')).toContainText('side channel');

    await ctxA.close();
    await ctxB.close();
  } finally {
    await stopHopd(hopd);
  }
});
