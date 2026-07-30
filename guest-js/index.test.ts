import {
   getCover,
   getThumbnails,
   type CoverInfo,
   type ThumbnailInfo,
} from './index';

function expectType<T>(_value: T): void {}

expectType<Promise<CoverInfo | null>>(getCover('/video.mp4'));
expectType<Promise<ThumbnailInfo[]>>(
   getThumbnails('/video.mp4', {
      timestamps: [0, 250],
      trackId: 3,
      accurate: true,
      headers: { Authorization: 'Bearer token' },
   }),
);

declare const cover: CoverInfo;
expectType<Uint8Array>(cover.data);
