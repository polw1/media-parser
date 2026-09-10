import {
   getCover,
   getSubtitles,
   getThumbnails,
   type CoverInfo,
   type SubtitleCueInfo,
   type SubtitleInfo,
   type SubtitleOptions,
   type ThumbnailInfo,
} from './index';

function expectType<T>(_value: T): void {}

expectType<Promise<CoverInfo | null>>(getCover('/video.mp4'));
expectType<Promise<ThumbnailInfo[]>>(
   getThumbnails('/video.mp4', {
      timestamps: [0, 250],
      trackId: 3,
      accurate: true,
      maxWidth: 640,
      maxHeight: 360,
      headers: { Authorization: 'Bearer token' },
   }),
);

declare const cover: CoverInfo;
expectType<Uint8Array>(cover.data);

expectType<Promise<SubtitleInfo[]>>(
   getSubtitles('/video.mp4', {
      trackId: 0,
      language: '',
      startMs: 0,
      endMs: Number.MAX_SAFE_INTEGER,
      headers: { Authorization: 'Bearer token' },
   }),
);

const subtitleOptions: SubtitleOptions = {
   language: 'eng',
   startMs: 250,
   endMs: 1_000,
};
expectType<Promise<SubtitleInfo[]>>(getSubtitles('/video.mp4', subtitleOptions));

declare const subtitle: SubtitleInfo;
expectType<number>(subtitle.id);
expectType<string>(subtitle.codec);
expectType<string | undefined>(subtitle.language);
expectType<number>(subtitle.timescale);
expectType<number>(subtitle.duration);
expectType<SubtitleCueInfo[]>(subtitle.cues);

declare const cue: SubtitleCueInfo;
expectType<number>(cue.cueId);
expectType<number>(cue.startSec);
expectType<number>(cue.endSec);
expectType<string>(cue.text);
