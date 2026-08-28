import { test, expect } from '@playwright/test';
import { openTab, startHopd, stopHopd } from '../helpers';

test('todo: two tabs sync and the tape survives restart', async ({ browser }) => {
  const hopd = await startHopd('hoprt/hop/todo.hop', 19100, 19101);
  try {
    const ctxA = await browser.newContext();
    const ctxB = await browser.newContext();
    const a = await ctxA.newPage();
    const b = await ctxB.newPage();
    await openTab(a, hopd.url);
    await openTab(b, hopd.url);

    await a.locator('#draft').fill('buy milk');
    await a.locator('#add').click();
    await expect(a.locator('#todos li')).toHaveText(['buy milk']);
    await expect(b.locator('#todos li')).toHaveText(['buy milk']);
    await expect(a.locator('#stats')).toContainText('0 done of 1');

    await a.locator('#todos li').click();
    await expect(a.locator('#todos li')).toHaveClass(/done/);
    await expect(b.locator('#todos li')).toHaveClass(/done/);
    await expect(b.locator('#stats')).toContainText('1 done of 1');

    await ctxA.close();
    await ctxB.close();
    await stopHopd(hopd);

    const hopd2 = await startHopd('hoprt/hop/todo.hop', 19100, 19101, hopd.dataDir);
    try {
      const ctx = await browser.newContext();
      const page = await ctx.newPage();
      await openTab(page, hopd2.url);
      await expect(page.locator('#todos li')).toHaveText(['buy milk']);
      await expect(page.locator('#todos li')).toHaveClass(/done/);
      await ctx.close();
    } finally {
      await stopHopd(hopd2);
    }
  } catch (e) {
    await stopHopd(hopd);
    throw e;
  }
});
