/**
 * GSAP 单点注册（规范见 DESIGN.md §3）。
 * 全仓库只允许从这里 import gsap 与插件，保证插件只注册一次、品牌缓动处处一致。
 * gsap 3.13+ 起全部插件免费（含 SplitText / MorphSVG / ScrambleText / Physics2D）。
 */
import gsap from 'gsap';
import { Flip } from 'gsap/Flip';
import { SplitText } from 'gsap/SplitText';
import { ScrambleTextPlugin } from 'gsap/ScrambleTextPlugin';
import { DrawSVGPlugin } from 'gsap/DrawSVGPlugin';
import { MorphSVGPlugin } from 'gsap/MorphSVGPlugin';
import { MotionPathPlugin } from 'gsap/MotionPathPlugin';
import { Physics2DPlugin } from 'gsap/Physics2DPlugin';
import { ScrollToPlugin } from 'gsap/ScrollToPlugin';
import { CustomEase } from 'gsap/CustomEase';
import { CustomBounce } from 'gsap/CustomBounce';
import { CustomWiggle } from 'gsap/CustomWiggle';
import { useGSAP } from '@gsap/react';

gsap.registerPlugin(
  useGSAP,
  Flip,
  SplitText,
  ScrambleTextPlugin,
  DrawSVGPlugin,
  MorphSVGPlugin,
  MotionPathPlugin,
  Physics2DPlugin,
  ScrollToPlugin,
  CustomEase,
  CustomBounce,
  CustomWiggle,
);

/** 品牌缓动：ch-out 主缓动 / ch-bounce 落地回弹 / ch-wiggle 错误摇晃 */
CustomEase.create('ch-out', '0.22,1,0.36,1');
CustomBounce.create('ch-bounce', { strength: 0.45, squash: 0 });
CustomWiggle.create('ch-wiggle', { wiggles: 6, type: 'easeOut' });

gsap.defaults({ duration: 0.56, ease: 'ch-out' });

export { gsap, Flip, SplitText, useGSAP };
