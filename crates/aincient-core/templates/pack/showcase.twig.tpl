{# Token-routed styling only — colours and sizes come from the design tokens
   (var(--…)) so a site rebrand reaches this markup with no pack change. #}
<section class="showcase showcase--{{ variant|default('centered') }}" data-tone="{{ tone|default('default') }}">
  {% if eyebrow %}<p class="showcase__eyebrow">{{ eyebrow }}</p>{% endif %}
  {% if heading %}<h2 class="showcase__heading">{{ heading }}</h2>{% endif %}
  {% if claim %}<p class="showcase__claim">{{ claim }}</p>{% endif %}
  {% if cta_label and cta_url %}<a class="showcase__cta" href="{{ cta_url }}">{{ cta_label }}</a>{% endif %}
</section>
