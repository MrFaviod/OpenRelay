/* Repro público: alcanza el desborde mediante push/drain reales, sin seed privado. */
#include "capture-spsc.h"
#include <stdint.h>
#include <stdio.h>
int main(void) {
    arl_spsc *r=NULL;
    if(arl_spsc_create(&r)) return 2;
    int16_t filler[960],input[32],out[32];
    for(unsigned i=0;i<960;i++) filler[i]=(int16_t)(-6000+(int)i);
    uint64_t left=(uint64_t)UINT32_MAX-15u;
    while(left) {
        uint32_t n=left>960?960:(uint32_t)left,d=0;
        arl_spsc_push_result p;
        if(arl_spsc_push(r,filler,n,&p)||p.written!=n||p.dropped||arl_spsc_drain(r,n,&d)||d!=n) return 3;
        left-=n;
    }
    for(unsigned i=0;i<32;i++)input[i]=(int16_t)(100+i);
    arl_spsc_push_result p;
    uint32_t a=0,b=0;
    if(arl_spsc_push(r,input,32,&p)||p.written!=32||p.dropped||arl_spsc_read(r,out,16,&a)||a!=16||arl_spsc_read(r,out+16,16,&b)||b!=16)return 4;
    unsigned bad=0;
    for(unsigned i=0;i<32;i++) if(out[i]!=input[i]) {
        if(bad<4) printf("MISMATCH i=%u expected=%d actual=%d\n",i,input[i],out[i]);
        bad++;
    }
    printf("PUBLIC_WRAP written=%u read=%u mismatches=%u progressed=%llu\n",p.written,a+b,bad,(unsigned long long)((uint64_t)UINT32_MAX-15u));
    arl_spsc_destroy(&r);
    return bad?1:0;
}
