<script lang="ts">
  import { t } from '../lib/i18n';
  import { shortDomain } from '../lib/model';
  import { useResource } from '../lib/resource.svelte';
  import { toast } from '../lib/toast.svelte';
  import type { ProxyLink, PublicDomain } from '../lib/types';
  import Icon from '../shared/Icon.svelte';
  import PublicShell from './PublicShell.svelte';

  // Links to connect: every domain has an "open in Telegram" link, a QR code and a copy button.
  // Opening and the QR code need no script; only copying does.
  let { fetchFn }: { fetchFn?: typeof fetch } = $props();

  const links = useResource<ProxyLink[]>(() => '/api/v1/links', fetchFn);
  const domains = useResource<PublicDomain[]>(() => '/api/v1/domains', fetchFn);
  const steps = ['one', 'two', 'three'] as const;

  // «Порт 443, мастер alpha»: the port is the one in the link; a folded domain says where it runs.
  function subline(link: ProxyLink): string | null {
    const d = domains.data?.find((x) => x.domain === link.domain);
    if (!d) return null;
    const port = new URLSearchParams(link.link.split('?')[1] ?? '').get('port') ?? '443';
    if (d.folded_into) return t('links.sub.folded', { port, domain: shortDomain(d.folded_into) });
    if (d.masters.length === 0) return null;
    const nodes = d.masters.join(', ');
    return t(d.masters.length > 1 ? 'links.sub.masters' : 'links.sub.master', { port, nodes });
  }

  async function copy(link: ProxyLink): Promise<void> {
    try {
      await navigator.clipboard.writeText(link.link);
      toast(t('links.copied', { name: shortDomain(link.domain) }));
    } catch {
      toast(t('links.copyFailed'));
    }
  }
</script>

<PublicShell active="links" updatedAt={links.updatedAt} stale={links.stale}>
  <div class="ph">
    <h1>{t('links.title')}</h1>
    <span class="sub">{t('links.sub')}</span>
  </div>
  <div class="stack">
    {#if links.stale}<p class="stale">{t('common.stale')}</p>{/if}

    <section class="t" style="--i:0">
      <div class="steps">
        {#each steps as step, i (step)}
          <div class="stp">
            <span class="n">{i + 1}</span>
            <b class="b">{t(`links.step.${step}`)}</b>
            <span class="muted sm">{t(`links.step.${step}Text`)}</span>
          </div>
        {/each}
      </div>
    </section>

    {#if links.data && links.data.length === 0}
      <section class="t empty" style="--i:1">
        <h2>{t('links.empty.title')}</h2>
        <p>{t('links.empty.text')}</p>
      </section>
    {:else if links.data}
      <div class="g4">
        {#each links.data as link, i (link.id)}
          <section class="t dom" style="--i:{i + 1}">
            <div class="qrw">
              <div class="qr">
                <img class="qr-img" src={link.qr_url} alt={t('links.qr', { domain: link.domain })} width="132" height="132" loading="lazy" />
              </div>
              <div class="stack" style="gap:10px">
                <div>
                  <div class="d">{link.domain}</div>
                  {#if subline(link)}<div class="sub-l muted sm">{subline(link)}</div>{/if}
                </div>
                <div class="row w">
                  <a class="btn pri s" href={link.link}><Icon name="send" />{t('links.open.button')}</a>
                  <button class="btn s" type="button" onclick={() => copy(link)}><Icon name="copy" />{t('links.copy')}</button>
                </div>
              </div>
            </div>
          </section>
        {/each}
      </div>
      <p class="faint sm" style="max-width:70ch">{t('links.note')}</p>
    {:else if !links.stale}
      <div class="g4"><section class="t dom"><div class="sk" style="height:132px"></div></section></div>
    {/if}
  </div>
</PublicShell>
