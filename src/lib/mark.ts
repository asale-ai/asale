// The brand mark as raw path data, traced from `logo.svg` at the repo root.
//
// KEEP IN SYNC with asale-web/src/lib/mark.ts.
//
// It lives on its own, with no imports, because three very different renderers
// need the same artwork: the share card draws it into a canvas with `Path2D`,
// the Open Graph images embed it as an `<img>` because Satori cannot be handed
// a canvas, and neither can reach for the `.svg` file — one runs in the
// browser, the other inside a build.
//
// `BOX` is the glyph's own bounding box *after* `GROUP`, not the 1024 viewBox
// it was exported in: the artwork sits off-centre in that square, so anything
// drawing the viewBox gets a mark floating in a margin nobody asked for.

export const MARK_PATHS = [
  "M 376.89 615.55 Q 376.91 615.54 377.87 615.86 Q 378.39 616.03 378.82 616.01 C 385.40 615.81 391.40 616.32 397.74 616.25 Q 406.15 616.16 472.22 616.07 A 0.82 0.82 0.0 0 0 472.92 615.68 L 568.62 465.85 A 0.61 0.61 0.0 0 0 568.34 464.96 Q 557.49 460.47 545.39 457.37 Q 533.88 454.43 521.55 453.61 C 515.45 453.20 507.89 453.07 500.82 453.70 Q 454.28 457.85 419.06 487.12 Q 410.55 494.19 402.18 504.09 Q 391.93 516.20 387.58 523.09 Q 371.13 549.15 370.32 550.45 A 0.50 0.50 0.0 0 1 369.47 550.45 Q 361.41 537.63 353.71 523.55 C 352.43 521.21 350.19 518.49 348.70 515.91 Q 338.19 497.73 337.74 496.99 C 333.45 489.96 329.78 482.66 328.21 473.41 Q 327.48 469.07 327.95 458.98 C 328.47 448.04 332.00 439.43 338.43 429.54 C 343.68 421.48 347.55 413.41 352.96 404.86 C 359.17 395.06 365.82 383.16 371.85 373.18 Q 399.33 327.68 423.86 286.11 Q 429.70 276.21 435.32 268.56 C 440.03 262.15 445.41 258.82 453.24 254.26 C 462.10 249.10 474.21 246.52 483.76 246.54 Q 524.65 246.60 564.16 246.41 A 0.35 0.35 0.0 0 1 564.47 246.93 Q 560.08 254.95 554.31 263.56 Q 549.89 270.15 539.63 288.41 C 527.90 309.29 523.06 336.88 527.67 360.90 Q 530.94 377.99 536.71 390.04 C 543.63 404.51 555.81 421.56 562.48 432.82 C 564.44 436.12 567.61 440.21 570.09 444.15 Q 588.54 473.39 605.67 499.85 Q 617.53 518.16 630.64 539.34 A 0.26 0.26 0.0 0 1 630.41 539.74 C 621.53 539.40 613.00 538.87 605.14 540.09 Q 600.65 540.78 594.44 542.19 C 580.26 545.41 567.87 552.26 556.41 561.19 Q 554.54 562.65 548.59 568.59 Q 542.19 574.98 538.60 580.60 Q 526.12 600.18 517.54 614.75 A 0.78 0.78 0.0 0 0 518.19 615.93 Q 528.09 616.22 534.00 616.21 Q 613.66 616.09 703.58 616.19 C 718.05 616.20 732.08 615.63 744.27 616.08 C 755.58 616.50 763.60 615.94 775.17 615.89 A 0.26 0.25 24.8 0 1 775.34 616.34 Q 773.81 617.63 771.60 618.57 C 758.95 623.96 746.48 630.36 733.60 635.57 Q 731.89 636.26 730.13 639.39 C 726.80 645.30 724.22 650.92 719.95 656.07 C 710.04 668.01 698.07 676.49 683.04 679.99 Q 676.35 681.55 661.83 681.55 Q 554.02 681.49 492.16 681.54 Q 476.41 681.55 469.60 680.04 C 454.59 676.70 440.98 667.67 431.96 655.41 Q 427.13 648.84 421.86 638.49 Q 420.90 636.61 419.11 635.89 Q 415.60 634.49 386.49 621.01 Q 385.29 620.46 380.54 618.71 Q 379.62 618.37 376.80 616.32 A 0.45 0.44 -38.5 0 1 376.89 615.55 Z",
  "M 588.78 246.45 Q 597.84 246.63 661.17 246.47 Q 675.32 246.43 682.24 247.99 C 698.60 251.68 713.30 261.27 722.15 275.87 Q 746.44 315.95 775.51 365.18 C 780.04 372.84 785.83 382.21 790.41 390.24 C 795.48 399.12 801.27 407.50 806.74 417.35 Q 811.37 425.68 817.80 436.20 C 823.70 445.85 825.03 458.91 824.12 470.72 Q 823.18 482.89 817.28 492.53 C 810.12 504.22 804.89 514.35 797.86 525.52 Q 791.75 535.23 783.37 549.94 A 1.13 1.12 44.4 0 1 781.43 549.96 C 779.44 546.67 777.24 543.74 775.47 540.70 C 769.32 530.13 762.52 520.31 756.71 511.11 C 753.30 505.71 747.81 496.83 743.28 490.06 C 733.00 474.70 724.89 460.80 714.40 444.80 Q 705.83 431.72 695.72 415.45 C 689.78 405.89 683.71 397.30 678.20 388.23 Q 672.46 378.79 667.61 371.83 C 663.01 365.23 658.69 357.25 653.78 349.88 C 643.11 333.86 631.59 314.65 619.86 297.39 C 614.80 289.93 609.97 281.24 604.91 273.65 Q 598.20 263.59 588.26 247.35 A 0.59 0.59 0.0 0 1 588.78 246.45 Z",
];

/** The group transform the paths were exported under. */
export const MARK_GROUP = { tx: -247.04, ty: -99.16, s: 1.3172 };

/** Glyph bounding box in user space, after `MARK_GROUP`. */
export const MARK_BOX = { x: 184.4, y: 225.5, w: 655.3, h: 573.0 };

/**
 * The mark as a standalone SVG document, cropped to the glyph.
 *
 * The crop is done by translating the artwork to the origin, not by giving the
 * viewBox a non-zero `min-x`/`min-y`, and the root carries explicit `width` and
 * `height`. Both matter: Satori (which draws the OG images) inlines an SVG data
 * URI rather than rendering it as a document, and an offset viewBox comes out
 * as an empty box of the right size — the mark silently vanishes and only the
 * gap where it should have been survives.
 */
export function markSvg(fill: string): string {
  const { x, y, w, h } = MARK_BOX;
  const { tx, ty, s } = MARK_GROUP;
  return (
    `<svg xmlns="http://www.w3.org/2000/svg" width="${w}" height="${h}" viewBox="0 0 ${w} ${h}">` +
    `<g fill="${fill}" transform="translate(${-x} ${-y}) translate(${tx} ${ty}) scale(${s})">` +
    MARK_PATHS.map((d) => `<path d="${d}"/>`).join("") +
    `</g></svg>`
  );
}
