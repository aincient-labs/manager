# A starter section — rename it to your first real component (the machine name
# is the DIRECTORY + file name; component names are globally unique per site).
# The `thirdPartySettings.atelier` block is what admits it to the catalog:
# run `atelier pack validate` after every change here.
name: Showcase
status: experimental
group: '__LABEL__'
description: 'A headline, a claim and one call to action.'
props:
  type: object
  properties:
    variant:
      type: string
      enum: [centered, split]
      default: centered
    tone:
      type: string
      enum: [default, muted, brand, inverted]
      default: default
    eyebrow: { type: string }
    heading: { type: string }
    claim: { type: string }
    cta_label: { type: string }
    cta_url: { type: string }
thirdPartySettings:
  atelier:
    api: 1
    tier: section
    order: 50
    icon: ◇
    # The one-line selection hint — the ONLY reason the agent will ever place
    # this component. Mandatory. Say when to use it, not what it is.
    use: 'A bold standalone claim with one call to action. Use for a single mid-page message.'
    props:
      variant: centered|split
      tone: ''
      eyebrow: ''
      heading: ''
      claim: ''
      cta_label: ''
      cta_url: ''
    prop_vocab:
      claim: 'the single bold claim sentence under the heading.'
    stylesheet: assets/__MODULE__.css
    # Declared render fixtures: the gallery renders these, the agent learns
    # from them, visual regression baselines on them. Keep at least one.
    examples:
      - name: default
        props:
          variant: centered
          tone: default
          eyebrow: 'Why us'
          heading: 'The claim, made visible'
          claim: 'One bold sentence that earns the click.'
          cta_label: 'See how'
          cta_url: 'https://example.com'
